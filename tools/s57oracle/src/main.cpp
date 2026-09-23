// s57oracle — S-57 reader oracle.
//
// Links OpenCPN's S-57 reader (libs/iso8211 + libs/s57-charts + the GDAL/OGR
// subset in libs/gdal) and dumps, feature by feature, what that reader makes of
// an ENC cell *after its update files are applied the way OpenCPN applies
// them*. NavCore's own S-57 reader is diffed against this output.
//
//     s57oracle [--no-updates] [--s57data DIR] <cell.000>   > out.ndjson
//
// Output: NDJSON on stdout. Line 1 is {"dsid":{...}}, then one line per
// feature in the reader's order — oFE_Index order, which is ascending FRID
// RCID (DDFRecordIndex qsorts by key), not file order; features inserted by
// updates land at their RCID. Diagnostics go to stderr.
//
// The ingest sequence mirrors Osenc::ingestCell / Osenc::ValidateAndCountUpdates
// / s57chart::GetUpdateFileArray (gui/src/Osenc.cpp, gui/src/s57chart.cpp) —
// see README.md for the reader options and why.
//
// Dev tool only — it links GPL code and must never be shipped with navcore.

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <string>
#include <utility>
#include <vector>

#include "gdal/cpl_conv.h"
#include "gdal/cpl_string.h"
#include "gdal/ogr_api.h"
#include "gdal/ogr_feature.h"
#include "gdal/ogr_geometry.h"
#include "gdal/ogrsf_frmts.h"
#include "iso8211.h"
#include "s57class_registrar.h"

// S57Reader keeps the post-update feature records (oFE_Index), the DSPM
// multipliers and the lexical levels private. The oracle needs the records to
// report attribute values exactly as stored (OGR converts E/I/F attributes to
// int/double), so it opens the class up in this one translation unit. Every
// header the class depends on is already included above, so the redefinition
// touches nothing but S57Reader / DDFRecordIndex / S57Writer. (`class` ->
// `struct` because most members sit before any access specifier.)
#define class struct
#define private public
#include "s57.h"
#undef private
#undef class
#include "ogr_s57.h"

namespace fs = std::filesystem;

#ifndef OCPN_S57DATA
#define OCPN_S57DATA ""
#endif

// ---------------------------------------------------------------------------
// JSON output helpers
// ---------------------------------------------------------------------------

static void json_str(std::string &out, const std::string &utf8) {
  out += '"';
  for (unsigned char c : utf8) {
    switch (c) {
      case '"': out += "\\\""; break;
      case '\\': out += "\\\\"; break;
      case '\n': out += "\\n"; break;
      case '\r': out += "\\r"; break;
      case '\t': out += "\\t"; break;
      default:
        if (c < 0x20 || c == 0x7f) {
          char buf[8];
          snprintf(buf, sizeof buf, "\\u%04x", c);
          out += buf;
        } else {
          out += (char)c;
        }
    }
  }
  out += '"';
}

static void utf8_push(std::string &out, unsigned cp) {
  if (cp < 0x80) {
    out += (char)cp;
  } else if (cp < 0x800) {
    out += (char)(0xC0 | (cp >> 6));
    out += (char)(0x80 | (cp & 0x3F));
  } else if (cp < 0x10000) {
    out += (char)(0xE0 | (cp >> 12));
    out += (char)(0x80 | ((cp >> 6) & 0x3F));
    out += (char)(0x80 | (cp & 0x3F));
  } else {
    out += (char)(0xF0 | (cp >> 18));
    out += (char)(0x80 | ((cp >> 12) & 0x3F));
    out += (char)(0x80 | ((cp >> 6) & 0x3F));
    out += (char)(0x80 | (cp & 0x3F));
  }
}

// Lexical level 0 (ASCII) and 1 (ISO 8859-1) are both decoded as Latin-1: for
// pure ASCII that is the identity, and it keeps the output valid UTF-8 if a
// level-0 cell carries stray high bytes.
static std::string latin1_to_utf8(const char *s) {
  std::string out;
  for (const unsigned char *p = (const unsigned char *)s; *p; ++p) utf8_push(out, *p);
  return out;
}

// Lexical level 2: UCS-2 little-endian. S-57 terminates such strings with the
// unit terminator (0x1F 0x00); OpenCPN strips it (att_conv.RemoveLast()).
static std::string ucs2le_to_utf8(const unsigned char *p, int nbytes) {
  std::string out;
  for (int i = 0; i + 1 < nbytes; i += 2) {
    unsigned cu = p[i] | (p[i + 1] << 8);
    if (cu == 0) break;
    if (cu == 0x1F && i + 2 >= nbytes - 1) break;
    if (cu >= 0xD800 && cu <= 0xDBFF && i + 3 < nbytes) {
      unsigned lo = p[i + 2] | (p[i + 3] << 8);
      if (lo >= 0xDC00 && lo <= 0xDFFF) {
        utf8_push(out, 0x10000 + ((cu - 0xD800) << 10) + (lo - 0xDC00));
        i += 2;
        continue;
      }
    }
    utf8_push(out, cu);
  }
  while (!out.empty() && out.back() == 0x1F) out.pop_back();
  return out;
}

// Coordinates are integers / COMF, so printing with log10(COMF) decimals is
// exact; trailing zeros are trimmed.
static int g_coord_decimals = 7;
static int g_depth_decimals = 1;

static void num(std::string &out, double v, int decimals) {
  char buf[64];
  snprintf(buf, sizeof buf, "%.*f", decimals, v);
  char *dot = strchr(buf, '.');
  if (dot) {
    char *end = buf + strlen(buf) - 1;
    while (end > dot && *end == '0') *end-- = '\0';
    if (end == dot) *end = '\0';
  }
  if (strcmp(buf, "-0") == 0) strcpy(buf, "0");
  out += buf;
}

static int decimals_for(int mult) {
  int d = 0;
  long m = mult;
  while (m >= 10 && m % 10 == 0) { m /= 10; d++; }
  return m == 1 ? d : 9;  // not a power of ten: fall back to plenty
}

static void lonlat(std::string &out, double x, double y) {
  out += '[';
  num(out, x, g_coord_decimals);
  out += ',';
  num(out, y, g_coord_decimals);
  out += ']';
}

static void line_coords(std::string &out, OGRLineString *ls) {
  out += '[';
  for (int i = 0; i < ls->getNumPoints(); i++) {
    if (i) out += ',';
    lonlat(out, ls->getX(i), ls->getY(i));
  }
  out += ']';
}

static void geometry(std::string &out, OGRGeometry *g) {
  if (!g) {
    out += "null";
    return;
  }
  switch (wkbFlatten(g->getGeometryType())) {
    case wkbPoint: {
      OGRPoint *p = (OGRPoint *)g;
      out += "{\"type\":\"Point\",\"c\":";
      lonlat(out, p->getX(), p->getY());
      out += '}';
      break;
    }
    case wkbMultiPoint: {
      OGRMultiPoint *mp = (OGRMultiPoint *)g;
      out += "{\"type\":\"MultiPoint\",\"c\":[";
      for (int i = 0; i < mp->getNumGeometries(); i++) {
        OGRPoint *p = (OGRPoint *)mp->getGeometryRef(i);
        if (i) out += ',';
        out += '[';
        num(out, p->getX(), g_coord_decimals);
        out += ',';
        num(out, p->getY(), g_coord_decimals);
        out += ',';
        num(out, p->getZ(), g_depth_decimals);
        out += ']';
      }
      out += "]}";
      break;
    }
    case wkbLineString:
      out += "{\"type\":\"LineString\",\"c\":";
      line_coords(out, (OGRLineString *)g);
      out += '}';
      break;
    case wkbMultiLineString: {
      OGRMultiLineString *ml = (OGRMultiLineString *)g;
      out += "{\"type\":\"MultiLineString\",\"c\":[";
      for (int i = 0; i < ml->getNumGeometries(); i++) {
        if (i) out += ',';
        line_coords(out, (OGRLineString *)ml->getGeometryRef(i));
      }
      out += "]}";
      break;
    }
    case wkbPolygon: {
      OGRPolygon *poly = (OGRPolygon *)g;
      out += "{\"type\":\"Polygon\",\"rings\":[";
      if (poly->getExteriorRing()) {
        line_coords(out, poly->getExteriorRing());
        for (int i = 0; i < poly->getNumInteriorRings(); i++) {
          out += ',';
          line_coords(out, poly->getInteriorRing(i));
        }
      }
      out += "]}";
      break;
    }
    default:
      fprintf(stderr, "s57oracle: FATAL: unexpected OGR geometry type %d\n",
              (int)g->getGeometryType());
      abort();
  }
}

// ---------------------------------------------------------------------------
// Base-cell attributes — Osenc::GetBaseFileAttr
// ---------------------------------------------------------------------------

struct BaseAttr {
  std::string isdt = "20000101";  // backstop, as OpenCPN
  std::string edtn = "1";         // backstop, as OpenCPN
  long updn = 0;
};

static bool valid_date(const std::string &s) {
  if (s.size() != 8) return false;
  for (char c : s)
    if (c < '0' || c > '9') return false;
  int m = atoi(s.substr(4, 2).c_str()), d = atoi(s.substr(6, 2).c_str());
  return m >= 1 && m <= 12 && d >= 1 && d <= 31;
}

static bool read_dsid(const std::string &path, std::string *isdt, std::string *edtn,
                      long *updn) {
  DDFModule m;
  if (!m.Open(path.c_str(), TRUE)) return false;
  m.Rewind();
  DDFRecord *pr = m.ReadRecord();  // record 0
  if (!pr) return false;
  const char *u = pr->GetStringSubfield("DSID", 0, "ISDT", 0);
  if (u && strlen(u)) *isdt = u;
  u = pr->GetStringSubfield("DSID", 0, "EDTN", 0);
  if (u && strlen(u) && edtn) *edtn = u;
  if (updn) {
    u = pr->GetStringSubfield("DSID", 0, "UPDN", 0);
    if (u) *updn = strtol(u, nullptr, 10);
  }
  return true;
}

// ---------------------------------------------------------------------------
// Update discovery — s57chart::GetUpdateFileArray + ValidateAndCountUpdates
// ---------------------------------------------------------------------------

// wxString::ToLong: the whole extension must parse as a base-10 integer.
static bool ext_number(const fs::path &p, long *out) {
  std::string ext = p.extension().string();
  if (ext.size() < 2) return false;
  ext = ext.substr(1);
  char *end = nullptr;
  long v = strtol(ext.c_str(), &end, 10);
  if (end == ext.c_str() || *end) return false;
  *out = v;
  return true;
}

// Returns the numbered update files that qualify (edition equal to the base,
// issue date not earlier than the base), sorted by extension number.
static std::vector<std::pair<long, fs::path>> find_updates(const fs::path &cell000,
                                                           const BaseAttr &base) {
  std::vector<std::pair<long, fs::path>> ups;

  fs::path dir = cell000.parent_path();
  if (dir.empty()) dir = ".";
  bool recurse = false;
  // "If the directory one level above the .000 is perfectly numeric, the
  // dataset is presumed to hold each update in its own directory."
  {
    fs::path up = dir.parent_path();
    long tmp;
    std::string name = up.filename().string();
    char *end = nullptr;
    if (!name.empty()) {
      tmp = strtol(name.c_str(), &end, 10);
      (void)tmp;
      if (end != name.c_str() && !*end) {
        dir = up;
        recurse = true;
      }
    }
  }

  std::vector<fs::path> candidates;
  std::error_code ec;
  if (recurse) {
    for (auto &e : fs::recursive_directory_iterator(dir, ec))
      if (e.is_regular_file()) candidates.push_back(e.path());
  } else {
    for (auto &e : fs::directory_iterator(dir, ec))
      if (e.is_regular_file()) candidates.push_back(e.path());
  }

  std::string stem = cell000.stem().string();
  for (auto &f : candidates) {
    long n;
    if (!ext_number(f, &n) || f.stem().string() != stem) continue;
    std::string fname = f.filename().string();
    if (strcasecmp(fname.c_str(), "CATALOG.031") == 0) continue;

    std::string isdt = "20000101", edtn;
    if (!read_dsid(f.string(), &isdt, &edtn, nullptr)) {
      fprintf(stderr, "s57oracle: cannot open candidate update %s\n", f.c_str());
      continue;  // OpenCPN: umdate stays invalid -> compare fails -> not added
    }
    if (!valid_date(isdt)) isdt = "20000101";
    if (edtn.empty()) edtn = "1";
    if (isdt >= base.isdt && edtn == base.edtn) ups.push_back({n, f});
  }
  std::stable_sort(ups.begin(), ups.end(),
                   [](auto &a, auto &b) { return a.first < b.first; });
  return ups;
}

// ---------------------------------------------------------------------------
// Attributes as stored — ApplyObjectClassAttributes, reading the raw ATVL
// ---------------------------------------------------------------------------

using Attrs = std::vector<std::pair<std::string, std::string>>;

static void upsert(Attrs &a, const std::string &k, const std::string &v) {
  for (auto &kv : a)
    if (kv.first == k) {
      kv.second = v;
      return;
    }
  a.push_back({k, v});
}

static void erase(Attrs &a, const std::string &k) {
  a.erase(std::remove_if(a.begin(), a.end(), [&](auto &kv) { return kv.first == k; }),
          a.end());
}

// Mirrors the accept/skip rules of S57Reader::ApplyObjectClassAttributes
// exactly, but keeps the stored string instead of OGR's typed conversion.
static Attrs raw_attributes(S57Reader *rd, S57ClassRegistrar *reg, DDFRecord *rec,
                            OGRFeature *feat) {
  Attrs out;
  OGRFeatureDefn *defn = feat->GetDefnRef();

  if (DDFField *attf = rec->FindField("ATTF")) {
    int n = attf->GetRepeatCount();
    for (int i = 0; i < n; i++) {
      int id = rec->GetIntSubfield("ATTF", 0, "ATTL", i);
      const char *acr;
      if (id < 1 || id > reg->GetMaxAttrIndex() || (acr = reg->GetAttrAcronym(id)) == NULL)
        continue;
      const char *v = rec->GetStringSubfield("ATTF", 0, "ATVL", i);
      int iField = defn->GetFieldIndex(acr);
      if (iField < 0) continue;  // not in this class's schema: dropped
      if (v[0] == 0x7f) {        // deleted by update
        erase(out, acr);
        continue;
      }
      OGRFieldType t = defn->GetFieldDefn(iField)->GetType();
      if ((t == OFTInteger || t == OFTReal) && strlen(v) == 0 &&
          !(rd->nOptionFlags & S57M_PRESERVE_EMPTY_NUMBERS))
        continue;  // empty number left unset
      upsert(out, acr, latin1_to_utf8(v));
    }
  }

  if (DDFField *natf = rec->FindField("NATF")) {
    int n = natf->GetRepeatCount();
    for (int i = 0; i < n; i++) {
      int id = rec->GetIntSubfield("NATF", 0, "ATTL", i);
      const char *acr;
      // note: '>=' here where ATTF uses '>' — as in OpenCPN
      if (id < 1 || id >= reg->GetMaxAttrIndex() || (acr = reg->GetAttrAcronym(id)) == NULL)
        continue;
      if (feat->GetFieldIndex(acr) < 0) continue;
      const char *v = rec->GetStringSubfield("NATF", 0, "ATVL", i);
      if (v == NULL) continue;
      if (rd->Nall == 2) {
        int len = 0;
        const char *data = nullptr;
        DDFField *f = rec->FindField("NATF", 0);
        if (f) {
          DDFSubfieldDefn *sf = f->GetFieldDefn()->FindSubfieldDefn("ATVL");
          if (sf) {
            int maxlen = 0;
            data = f->GetSubfieldData(sf, &maxlen, i);
            len = sf->GetDataLength(data, maxlen, NULL);
          }
        }
        if (len) upsert(out, acr, ucs2le_to_utf8((const unsigned char *)data, len));
      } else {
        upsert(out, acr, latin1_to_utf8(v));  // no 0x7f check here, as in OpenCPN
      }
    }
  }
  return out;
}

static bool is_standard_field(const char *name) {
  static const char *std_fields[] = {"RCID", "PRIM", "GRUP", "OBJL", "RVER", "AGEN",
                                     "FIDN", "FIDS", "LNAM", "LNAM_REFS", "FFPT_RIND",
                                     "NAME_RCNM", "NAME_RCID", "ORNT", "USAG", "MASK",
                                     "DEPTH", nullptr};
  for (int i = 0; std_fields[i]; i++)
    if (strcmp(name, std_fields[i]) == 0) return true;
  return false;
}

// ---------------------------------------------------------------------------

static void usage() {
  fprintf(stderr,
          "usage: s57oracle [--no-updates] [--s57data DIR] <cell.000>\n"
          "  Dumps OpenCPN's S-57 reading of an ENC cell as NDJSON on stdout.\n");
  exit(2);
}

int main(int argc, char **argv) {
  bool apply_updates = true;
  std::string s57data = OCPN_S57DATA;
  std::string cell;

  for (int i = 1; i < argc; i++) {
    std::string a = argv[i];
    if (a == "--no-updates")
      apply_updates = false;
    else if (a == "--s57data" && i + 1 < argc)
      s57data = argv[++i];
    else if (a.size() && a[0] == '-')
      usage();
    else if (cell.empty())
      cell = a;
    else
      usage();
  }
  if (cell.empty()) usage();

  if (s57data.empty() || !fs::exists(fs::path(s57data) / "s57objectclasses.csv")) {
    fprintf(stderr, "s57oracle: s57objectclasses.csv not found in '%s' (use --s57data)\n",
            s57data.c_str());
    return 1;
  }

  // ---- base cell attributes (Osenc::GetBaseFileAttr) ----
  BaseAttr base;
  {
    std::string isdt = "20000101";
    if (!read_dsid(cell, &isdt, &base.edtn, &base.updn)) {
      fprintf(stderr, "s57oracle: cannot open %s\n", cell.c_str());
      return 1;
    }
    base.isdt = valid_date(isdt) ? isdt : "20000101";
  }

  // ---- class registrar: OpenCPN loads it from its s57data dir ----
  S57ClassRegistrar registrar;
  if (!registrar.LoadInfo(s57data.c_str(), TRUE)) {
    fprintf(stderr, "s57oracle: failed to load S-57 catalogue from %s\n", s57data.c_str());
    return 1;
  }

  // ---- open + ingest (Osenc::ingestCell) ----
  char **opts = NULL;
  opts = CSLSetNameValue(opts, S57O_RETURN_LINKAGES, "ON");
  opts = CSLSetNameValue(opts, S57O_RETURN_PRIMITIVES, "ON");

  OGRS57DataSource ds;
  ds.SetS57Registrar(&registrar);
  ds.SetOptionList(opts);
  if (ds.Open(cell.c_str(), TRUE, NULL)) {
    fprintf(stderr, "s57oracle: OGRS57DataSource::Open failed for %s\n", cell.c_str());
    return 1;
  }
  S57Reader *rd = ds.GetModule(0);

  // ---- updates ----
  int available = 0;          // number of the last qualifying update file
  long last_applied = base.updn;
  int applied_files = 0;
  std::string last_update_isdt = base.isdt;
  std::vector<std::string> applied_names;
  if (apply_updates) {
    auto ups = find_updates(cell, base);
    if (!ups.empty()) available = (int)ups.back().first;
    // ValidateAndCountUpdates walks 0..available; a number with no qualifying
    // file, or a file of <= 25 bytes, becomes an empty dummy module, i.e. a
    // no-op update. ingestCell stops at the first update that reports failure.
    for (int n = 1; n <= available; n++) {
      const fs::path *file = nullptr;
      for (auto &u : ups)
        if (u.first == n) file = &u.second;
      std::error_code ec;
      if (!file || fs::file_size(*file, ec) <= 25 || ec) {
        fprintf(stderr, "s57oracle: update %03d missing/short — OpenCPN substitutes a NULL update\n", n);
        last_applied = n;
        continue;
      }
      DDFModule up;
      if (!up.Open(file->c_str(), FALSE)) {
        fprintf(stderr, "s57oracle: cannot open update %s — stopping\n", file->c_str());
        break;
      }
      int r = rd->ApplyUpdates(&up, n);
      if (r) {
        fprintf(stderr, "s57oracle: update %s reported failure (%d) — stopping, as OpenCPN\n",
                file->c_str(), r);
        break;
      }
      last_applied = n;
      applied_files++;
      applied_names.push_back(file->filename().string());
      std::string d;
      if (read_dsid(file->string(), &d, nullptr, nullptr) && !d.empty()) last_update_isdt = d;
    }
  }

  // Clear RETURN_PRIMITIVES to fetch normal features (LNAM_REFS is dropped
  // too, since this option list never had it) — exactly as ingestCell does.
  opts = CSLSetNameValue(opts, S57O_RETURN_PRIMITIVES, "OFF");
  rd->SetOptions(opts);
  CSLDestroy(opts);

  g_coord_decimals = decimals_for(rd->nCOMF);
  g_depth_decimals = decimals_for(rd->nSOMF);

  // ---- DSID line ----
  {
    std::string o = "{\"dsid\":{\"dsnm\":";
    json_str(o, rd->GetDSNM() ? rd->GetDSNM() : "");
    o += ",\"edtn\":";
    json_str(o, base.edtn);
    o += ",\"updn\":" + std::to_string(last_applied);
    o += ",\"base_updn\":" + std::to_string(base.updn);
    o += ",\"isdt\":";
    json_str(o, base.isdt);
    o += ",\"last_update_isdt\":";
    json_str(o, last_update_isdt);
    o += ",\"comf\":" + std::to_string(rd->nCOMF);
    o += ",\"somf\":" + std::to_string(rd->nSOMF);
    o += ",\"cscl\":" + std::to_string(rd->nCSCL);
    o += ",\"nall\":" + std::to_string(rd->Nall);
    o += ",\"aall\":" + std::to_string(rd->Aall);
    o += ",\"updates_applied\":" + std::to_string(applied_files);
    o += ",\"update_files\":[";
    for (size_t i = 0; i < applied_names.size(); i++) {
      if (i) o += ',';
      json_str(o, applied_names[i]);
    }
    o += "],\"features\":" + std::to_string(rd->GetFeatureCount());
    o += "}}\n";
    fputs(o.c_str(), stdout);
  }

  // ---- features: ReadNextFeature(NULL) == ReadFeature(i) for i in order ----
  rd->Rewind();
  int n_out = 0, n_dropped = 0, n_nogeom = 0;
  int nfe = rd->GetFeatureCount();
  for (int i = 0; i < nfe; i++) {
    DDFRecord *rec = rd->oFE_Index.GetByIndex(i);
    OGRFeature *f = rd->ReadFeature(i, NULL);
    if (!f) {
      fprintf(stderr, "s57oracle: reader dropped feature RCID=%d OBJL=%d (no class definition)\n",
              rec->GetIntSubfield("FRID", 0, "RCID", 0),
              rec->GetIntSubfield("FRID", 0, "OBJL", 0));
      n_dropped++;
      continue;
    }

    Attrs attrs = raw_attributes(rd, &registrar, rec, f);

    // Self-check: the raw list must hold exactly the fields OGR has set.
    {
      OGRFeatureDefn *d = f->GetDefnRef();
      int nset = 0;
      for (int k = 0; k < d->GetFieldCount(); k++) {
        const char *nm = d->GetFieldDefn(k)->GetNameRef();
        if (is_standard_field(nm) || !f->IsFieldSet(k)) continue;
        nset++;
        bool found = false;
        for (auto &kv : attrs) found |= kv.first == nm;
        if (!found)
          fprintf(stderr, "s57oracle: WARNING RCID=%d: OGR has %s set, raw list does not\n",
                  f->GetFieldAsInteger("RCID"), nm);
      }
      if (nset != (int)attrs.size())
        fprintf(stderr, "s57oracle: WARNING RCID=%d: %d OGR fields set vs %zu raw attrs\n",
                f->GetFieldAsInteger("RCID"), nset, attrs.size());
    }

    std::string o = "{\"rcid\":" + std::to_string(rec->GetIntSubfield("FRID", 0, "RCID", 0));
    o += ",\"objl\":" + std::to_string(rec->GetIntSubfield("FRID", 0, "OBJL", 0));
    o += ",\"acronym\":";
    json_str(o, f->GetDefnRef()->GetName());
    o += ",\"prim\":" + std::to_string(rec->GetIntSubfield("FRID", 0, "PRIM", 0));
    o += ",\"grup\":" + std::to_string(rec->GetIntSubfield("FRID", 0, "GRUP", 0));
    o += ",\"rver\":" + std::to_string(rec->GetIntSubfield("FRID", 0, "RVER", 0));
    // FIDN is b14 (unsigned 32-bit); DDF/OGR hand it back as a signed int, so
    // OpenCPN sees FIDN > 2^31 as negative. The oracle prints the spec value.
    o += ",\"foid\":\"" + std::to_string(rec->GetIntSubfield("FOID", 0, "AGEN", 0)) + "-" +
         std::to_string((unsigned)rec->GetIntSubfield("FOID", 0, "FIDN", 0)) + "-" +
         std::to_string(rec->GetIntSubfield("FOID", 0, "FIDS", 0)) + "\"";
    o += ",\"attrs\":{";
    for (size_t k = 0; k < attrs.size(); k++) {
      if (k) o += ',';
      json_str(o, attrs[k].first);
      o += ':';
      json_str(o, attrs[k].second);
    }
    o += "},\"geom\":";
    OGRGeometry *g = f->GetGeometryRef();
    if (!g) n_nogeom++;
    geometry(o, g);
    o += "}\n";
    fputs(o.c_str(), stdout);
    n_out++;
    delete f;
  }

  fprintf(stderr,
          "s57oracle: %s: %d features out (%d without geometry), %d dropped by reader, "
          "%d update file(s) applied, UPDN %ld\n",
          cell.c_str(), n_out, n_nogeom, n_dropped, applied_files, last_applied);
  return 0;
}
