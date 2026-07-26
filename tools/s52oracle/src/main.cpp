// s52oracle — S-52 symbology-resolution oracle.
//
// Links OpenCPN's s52plib (the standalone presentation-library engine under
// libs/s52plib) and exposes it as a pure function:
//
//     (object class, primitive, attributes)  ->  LUP + expanded rule list
//
// Features arrive as NDJSON on stdin (produced by `navcore --dump-features`),
// the resolved instruction stream leaves as NDJSON on stdout. No chart I/O, no
// OpenGL, no wx frame: the oracle never parses an ENC, so navcore and OpenCPN
// are guaranteed to be resolving *the same* feature.
//
// Input line:
//   {"id":1,"obj":"DEPARE","prim":"A","attrs":{"DRVAL1":["F",2.0],"CATREA":["I",4]}}
// Output line:
//   {"id":1,"obj":"DEPARE","prim":"A","lup":{...},"rules":[...],"expanded":[...]}
//
// Dev tool only — it links GPLv2 code and must never be shipped with navcore.

#include <wx/wxprec.h>
#include <wx/wx.h>
#include <wx/init.h>
#include <wx/image.h>

#include <cstring>
#include <iostream>
#include <list>
#include <string>
#include <vector>

#include "s52plib.h"
#include "s52s57.h"
#include "s52utils.h"

#include "minijson.h"

// s52cnsy.cpp reaches for this global.
s52plib *ps52plib = nullptr;

// Defined in stubs.cpp; the plib appends "s57data" to it to find
// s57objectclasses.csv.
extern wxString g_SharedDataLocation;

// The CS procedure jump table, defined at the bottom of s52cnsy.cpp.
extern Cond condTable[];

// ---------------------------------------------------------------------------
// s57chart — the association hook.
//
// UDWHAZ03 decides whether an obstruction/wreck/rock is an *isolated danger*
// by asking the chart for the depth areas around it, through
// chart_context::pt2GetAssociatedObjects. s52plib only forward-declares
// s57chart, so the oracle supplies its own: navcore sends the surrounding
// DRVAL1/DRVAL2 values with each feature and this returns them as synthetic
// S57Objs. Without it the procedure takes its no-chart branch and can never
// confirm a danger.
// ---------------------------------------------------------------------------
class s57chart {
public:
  std::vector<S57Obj *> assoc;  // rebuilt per input record

  std::list<S57Obj *> *GetAssociatedObjects(S57Obj *) {
    auto *out = new std::list<S57Obj *>();  // _UDWHAZ03 deletes the list
    for (S57Obj *o : assoc) out->push_back(o);
    return out;
  }

  void clear() {
    for (S57Obj *o : assoc) delete o;
    assoc.clear();
  }
};

// ---------------------------------------------------------------------------
// S57Obj — minimal implementation.
//
// The real one lives in OpenCPN's gui/src/s57obj.cpp, which drags in the whole
// chart/GDAL/GL stack. Only the attribute store matters for symbology
// resolution, and its semantics are fixed by how FindBestLUP and the CS
// procedures read it: att_array is a packed array of 6-char acronyms, attVal a
// parallel array of typed values.
// ---------------------------------------------------------------------------

void S57Obj::Init() {
  att_array = NULL;
  attVal = NULL;
  n_attr = 0;
  pPolyTessGeo = NULL;
  bCS_Added = 0;
  CSrules = NULL;
  FText = NULL;
  bFText_Added = 0;
  geoPtMulti = NULL;
  geoPtz = NULL;
  geoPt = NULL;
  bIsClone = false;
  Scamin = 1e8 + 2;
  SuperScamin = -1;
  nRef = 0;
  bIsAton = false;
  bIsAssociable = false;
  m_n_lsindex = 0;
  m_lsindex_array = NULL;
  m_n_edge_max_points = 0;
  m_ls_list = 0;
  m_ls_list_legacy = 0;
  iOBJL = -1;
  bBBObj_valid = false;
  x_rate = 1.0;
  y_rate = 1.0;
  x_origin = 0.0;
  y_origin = 0.0;
  auxParm0 = 0;
  auxParm1 = 0;
  auxParm2 = 0;
  auxParm3 = 0;

  // Not touched by the upstream Init(), but the oracle constructs objects on
  // the stack, so leave nothing indeterminate.
  memset(FeatureName, 0, sizeof(FeatureName));
  Primitive_type = GEO_POINT;
  Index = -1;
  x = y = z = 0.0;
  npt = 0;
  m_lat = m_lon = 0.0;
  m_DisplayCat = DISPLAYBASE;
  m_DPRI = -1;
  m_bcategory_mutable = false;
  m_chart_context = NULL;
}

S57Obj::S57Obj() { Init(); }

S57Obj::S57Obj(const char *featureName) {
  Init();
  attVal = new wxArrayOfS57attVal();
  strncpy(FeatureName, featureName, 6);
  FeatureName[6] = 0;
  if (!strncmp(FeatureName, "DEPARE", 6) || !strncmp(FeatureName, "DRGARE", 6))
    bIsAssociable = true;
}

S57Obj::~S57Obj() {
  if (bIsClone) return;
  if (attVal) {
    for (unsigned int iv = 0; iv < attVal->GetCount(); iv++) {
      S57attVal *vv = attVal->Item(iv);
      free(vv->value);
      delete vv;
    }
    delete attVal;
  }
  free(att_array);
}

static void push_attr_name(S57Obj *o, const char *acronym) {
  o->att_array = (char *)realloc(o->att_array, 6 * (o->n_attr + 1));
  strncpy(o->att_array + (6 * o->n_attr), acronym, 6);
  o->n_attr++;
}

bool S57Obj::AddIntegerAttribute(const char *acronym, int val) {
  S57attVal *v = new S57attVal;
  int *p = (int *)malloc(sizeof(int));
  *p = val;
  v->valType = OGR_INT;
  v->value = p;
  push_attr_name(this, acronym);
  attVal->Add(v);
  if (!strncmp(acronym, "SCAMIN", 6)) Scamin = val;
  return true;
}

bool S57Obj::AddDoubleAttribute(const char *acronym, double val) {
  S57attVal *v = new S57attVal;
  double *p = (double *)malloc(sizeof(double));
  *p = val;
  v->valType = OGR_REAL;
  v->value = p;
  push_attr_name(this, acronym);
  attVal->Add(v);
  return true;
}

bool S57Obj::AddStringAttribute(const char *acronym, char *val) {
  S57attVal *v = new S57attVal;
  char *p = (char *)malloc(strlen(val) + 1);
  strcpy(p, val);
  v->valType = OGR_STR;
  v->value = p;
  push_attr_name(this, acronym);
  attVal->Add(v);
  return true;
}

bool S57Obj::AddIntegerListAttribute(const char *, int *, int) { return true; }
bool S57Obj::AddDoubleListAttribute(const char *, double *, int) { return true; }

int S57Obj::GetAttributeIndex(const char *AttrSeek) {
  char *patl = att_array;
  for (int i = 0; i < n_attr; i++) {
    if (!strncmp(patl, AttrSeek, 6)) return i;
    patl += 6;
  }
  return -1;
}

wxString S57Obj::GetAttrValueAsString(const char *AttrName) {
  wxString str;
  int idx = GetAttributeIndex(AttrName);
  if (idx >= 0) {
    S57attVal *v = attVal->Item(idx);
    switch (v->valType) {
      case OGR_STR:
        str.Append(wxString((char *)v->value, wxConvUTF8));
        break;
      case OGR_REAL:
        str.Printf("%g", *(double *)v->value);
        break;
      case OGR_INT:
        str.Printf("%d", *(int *)v->value);
        break;
      default:
        str.Printf("Unknown attribute type");
        break;
    }
  }
  return str;
}

// ---------------------------------------------------------------------------
// Oracle
// ---------------------------------------------------------------------------

struct Options {
  std::string plib_path;
  LUPname point_style = SIMPLIFIED;
  LUPname boundary_style = PLAIN_BOUNDARIES;
  double safety_contour = 10.0;
  double safety_depth = 6.0;
  double shallow_contour = 2.0;
  double deep_contour = 30.0;
  bool two_shades = false;
  bool shallow_pattern = false;
  int depth_unit = 1;  // 0=feet 1=metres 2=fathoms
  std::string color_scheme = "DAY";
  // Non-zero turns on the visibility verdict: the scale denominator of the view.
  double view_scale = 0.0;
  bool use_scamin = true;
  // OpenCPN's config default is 0 (navutil.cpp), so off unless asked for.
  bool use_super_scamin = false;
  int zoom_modifier = 0;
  // OpenCPN config default is 0 (navutil.cpp `Read("bShowMeta", &v, 0)`).
  bool show_meta = false;
  // ENC display category. navcore defaults to OTHER ("All"), which is what the
  // reference captures are taken with; the two must match or every OTHER-class
  // feature reads as a divergence that is really a configuration difference.
  DisCat display_cat = OTHER;
  // Directory containing s57data/ (s57objectclasses.csv).
  std::string shared_data = "/Applications/OpenCPN.app/Contents/SharedSupport/";
};

static const char *lupname_str(LUPname n) {
  switch (n) {
    case SIMPLIFIED: return "SIMPLIFIED";
    case PAPER_CHART: return "PAPER_CHART";
    case LINES: return "LINES";
    case PLAIN_BOUNDARIES: return "PLAIN_BOUNDARIES";
    case SYMBOLIZED_BOUNDARIES: return "SYMBOLIZED_BOUNDARIES";
    default: return "?";
  }
}

static const char *discat_str(DisCat c) {
  switch (c) {
    case DISPLAYBASE: return "DISPLAYBASE";
    case STANDARD: return "STANDARD";
    case OTHER: return "OTHER";
    case MARINERS_STANDARD: return "MARINERS_STANDARD";
    case MARINERS_OTHER: return "MARINERS_OTHER";
    default: return "?";
  }
}

// Split an S-52 instruction string ("AC(DEPMS);AP(DIAMOND1)") into tokens.
static std::vector<std::string> split_instructions(const std::string &s) {
  std::vector<std::string> out;
  std::string cur;
  for (char c : s) {
    if (c == ';' || c == '\037') {
      if (!cur.empty()) out.push_back(cur);
      cur.clear();
    } else if (c != '\n' && c != '\r') {
      cur.push_back(c);
    }
  }
  if (!cur.empty()) out.push_back(cur);
  return out;
}

// Call a conditional-symbology procedure by name. Returns false when the name
// is not in the jump table (OpenCPN would substitute QUESMRK there).
static bool call_cs(const std::string &name, ObjRazRules *rz, std::string &out) {
  for (int i = 0; condTable[i].condInst != NULL; i++) {
    if (name.compare(0, 8, condTable[i].name, 8) == 0) {
      void *ret = condTable[i].condInst((void *)rz);
      if (ret) {
        out.assign((char *)ret);
        free(ret);
      } else {
        out.clear();
      }
      return true;
    }
  }
  return false;
}

int main(int argc, char **argv) {
  Options opt;
  for (int i = 1; i < argc; i++) {
    std::string a = argv[i];
    auto next = [&]() -> std::string { return (i + 1 < argc) ? argv[++i] : ""; };
    if (a == "--plib") opt.plib_path = next();
    else if (a == "--points") opt.point_style = (next() == "paper") ? PAPER_CHART : SIMPLIFIED;
    else if (a == "--boundaries") opt.boundary_style = (next() == "symbolized") ? SYMBOLIZED_BOUNDARIES : PLAIN_BOUNDARIES;
    else if (a == "--safety-contour") opt.safety_contour = atof(next().c_str());
    else if (a == "--safety-depth") opt.safety_depth = atof(next().c_str());
    else if (a == "--shallow-contour") opt.shallow_contour = atof(next().c_str());
    else if (a == "--deep-contour") opt.deep_contour = atof(next().c_str());
    else if (a == "--two-shades") opt.two_shades = true;
    else if (a == "--shallow-pattern") opt.shallow_pattern = true;
    else if (a == "--depth-unit") opt.depth_unit = atoi(next().c_str());
    else if (a == "--color-scheme") opt.color_scheme = next();
    else if (a == "--shared-data") opt.shared_data = next();
    else if (a == "--view-scale") opt.view_scale = atof(next().c_str());
    else if (a == "--no-scamin") opt.use_scamin = false;
    else if (a == "--super-scamin") opt.use_super_scamin = true;
    else if (a == "--zoom-modifier") opt.zoom_modifier = atoi(next().c_str());
    else if (a == "--show-meta") opt.show_meta = true;
    else if (a == "--display-cat") {
      std::string v = next();
      opt.display_cat = (v == "base") ? DISPLAYBASE : (v == "standard") ? STANDARD : OTHER;
    }
    else {
      fprintf(stderr, "s52oracle: unknown option '%s'\n", a.c_str());
      return 2;
    }
  }
  if (opt.plib_path.empty()) {
    fprintf(stderr,
            "usage: s52oracle --plib <chartsymbols.xml> [--points simplified|paper]\n"
            "       [--boundaries plain|symbolized] [--safety-contour M] [--safety-depth M]\n"
            "       [--shallow-contour M] [--deep-contour M] [--two-shades]\n"
            "       [--shallow-pattern] [--depth-unit 0|1|2] < features.ndjson\n");
    return 2;
  }

  wxInitializer wxinit;
  if (!wxinit.IsOk()) {
    fprintf(stderr, "s52oracle: wxWidgets init failed\n");
    return 1;
  }
  wxImage::AddHandler(new wxPNGHandler);
  g_SharedDataLocation = wxString(opt.shared_data.c_str(), wxConvUTF8);

  // Mariner parameters must be set before the plib is built: the LUP tables
  // and CS results depend on them.
  S52_setMarinerParam(S52_MAR_SAFETY_CONTOUR, opt.safety_contour);
  S52_setMarinerParam(S52_MAR_SAFETY_DEPTH, opt.safety_depth);
  S52_setMarinerParam(S52_MAR_SHALLOW_CONTOUR, opt.shallow_contour);
  S52_setMarinerParam(S52_MAR_DEEP_CONTOUR, opt.deep_contour);
  S52_setMarinerParam(S52_MAR_TWO_SHADES, opt.two_shades ? 1.0 : 0.0);
  S52_setMarinerParam(S52_MAR_SHALLOW_PATTERN, opt.shallow_pattern ? 1.0 : 0.0);
  S52_setMarinerParam(S52_MAR_SYMBOLIZED_BND,
                      opt.boundary_style == SYMBOLIZED_BOUNDARIES ? 1.0 : 0.0);
  S52_setMarinerParam(S52_MAR_SYMPLIFIED_PNT,
                      opt.point_style == SIMPLIFIED ? 1.0 : 0.0);

  s52plib plib(wxString(opt.plib_path.c_str(), wxConvUTF8));
  if (!plib.m_bOK) {
    fprintf(stderr, "s52oracle: failed to load %s\n", opt.plib_path.c_str());
    return 1;
  }
  ps52plib = &plib;
  plib.m_nSymbolStyle = opt.point_style;
  plib.m_nBoundaryStyle = opt.boundary_style;
  plib.m_nDepthUnitDisplay = opt.depth_unit;
  plib.UpdateMarinerParams();
  plib.SetDisplayCategory(opt.display_cat);
  plib.m_bUseSCAMIN = opt.use_scamin;
  plib.m_bUseSUPER_SCAMIN = opt.use_super_scamin;
  // SCAMIN is scaled by the chart zoom modifier (pow(8, mod/5)); pin it so the
  // oracle's default is the neutral 1.0 rather than whatever the plib was
  // constructed with.
  plib.SetScaleFactorZoomMod(opt.zoom_modifier);
  plib.m_bShowMeta = opt.show_meta;

  // The viewport the visibility check is made against. Only the scale matters:
  // ObjectRenderCheckCat consults chart_scale for SCAMIN and SUPER_SCAMIN and
  // nothing else about the view.
  //
  // This must go through SetVPointCompat. PrepareForRender(vp) does *not* store
  // the viewport — it only feeds shader uniforms — so setting the scale that
  // way leaves vp_plib.chart_scale at 0, every `chart_scale > Scamin` test
  // false, and the oracle reporting every SCAMIN-filtered feature as visible.
  if (opt.view_scale > 0) {
    LLBBox bbox;
    bbox.Set(-90, -180, 90, 180);
    plib.SetVPointCompat(
        /*pix_width=*/2000, /*pix_height=*/1281,
        /*view_scale_ppm=*/1.0, /*rotation=*/0.0,
        /*clat=*/0.0, /*clon=*/0.0,
        /*chart_scale=*/opt.view_scale,
        wxRect(0, 0, 2000, 1281), bbox,
        /*ref_scale=*/opt.view_scale, /*display_scale=*/1.0);
  }

  // Chart context. `chart` points at the association hook above, so UDWHAZ03
  // sees whatever depth areas navcore reported around each feature. Features
  // that arrive without an "assoc" block get an empty list, which is the same
  // answer as "this object is not inside any depth area".
  wxArrayPtrVoid floating_atons, rigid_atons;
  s57chart assoc_chart;
  chart_context ctx;
  memset(&ctx, 0, sizeof(ctx));
  ctx.chart = &assoc_chart;
  ctx.pt2GetAssociatedObjects = &s57chart::GetAssociatedObjects;
  ctx.safety_contour = opt.safety_contour;
  ctx.pFloatingATONArray = &floating_atons;
  ctx.pRigidATONArray = &rigid_atons;
  ctx.chart_scale = 10000;
  ctx.chart_type = 0;

  std::string line;
  long nread = 0, nresolved = 0;
  while (std::getline(std::cin, line)) {
    if (line.empty()) continue;
    mj::Value rec;
    if (!mj::Parser(line).parse(rec) || rec.type != mj::Type::Obj) {
      fprintf(stderr, "s52oracle: bad input line: %s\n", line.c_str());
      continue;
    }
    nread++;

    const mj::Value *vid = rec.find("id");
    const mj::Value *vobj = rec.find("obj");
    const mj::Value *vprim = rec.find("prim");
    if (!vobj || !vprim) continue;
    // Pass the id through verbatim (navcore emits "<chart>#<index>" strings).
    std::string id_json;
    if (vid && vid->type == mj::Type::Str) {
      mj::esc(id_json, vid->str);
    } else if (vid) {
      id_json = std::to_string((long long)vid->num);
    } else {
      id_json = std::to_string(nread);
    }
    std::string objname = vobj->str;
    std::string prim = vprim->str;

    S57Obj obj(objname.c_str());
    obj.m_chart_context = &ctx;

    LUPname tname;
    if (prim == "P") {
      obj.Primitive_type = GEO_POINT;
      tname = opt.point_style;
    } else if (prim == "L") {
      obj.Primitive_type = GEO_LINE;
      tname = LINES;
    } else {
      obj.Primitive_type = GEO_AREA;
      tname = opt.boundary_style;
    }

    const mj::Value *attrs = rec.find("attrs");
    if (attrs && attrs->type == mj::Type::Obj) {
      for (const auto &kv : attrs->obj) {
        // Acronyms are always 6 chars in the S-57 model; pad short ones so the
        // packed att_array stays aligned.
        char acro[7] = {' ', ' ', ' ', ' ', ' ', ' ', 0};
        memcpy(acro, kv.first.c_str(), std::min<size_t>(6, kv.first.size()));
        const mj::Value &v = kv.second;
        if (v.type != mj::Type::Arr || v.arr.size() != 2) continue;
        const std::string &t = v.arr[0].str;
        const mj::Value &val = v.arr[1];
        if (t == "I") {
          obj.AddIntegerAttribute(acro, (int)val.num);
        } else if (t == "F") {
          obj.AddDoubleAttribute(acro, val.num);
        } else {
          std::string s = val.type == mj::Type::Str ? val.str : std::string();
          obj.AddStringAttribute(acro, (char *)s.c_str());
        }
      }
    }

    // Surrounding depth areas for UDWHAZ03.
    assoc_chart.clear();
    bool assoc_given = false;
    if (const mj::Value *assoc = rec.find("assoc")) {
      assoc_given = true;
      if (const mj::Value *areas = assoc->find("area_drval1")) {
        for (const mj::Value &v : areas->arr) {
          S57Obj *o = new S57Obj("DEPARE");
          o->Primitive_type = GEO_AREA;
          o->AddDoubleAttribute("DRVAL1", v.num);
          assoc_chart.assoc.push_back(o);
        }
      }
      if (const mj::Value *lines = assoc->find("line_drval2")) {
        for (const mj::Value &v : lines->arr) {
          S57Obj *o = new S57Obj("DEPARE");
          o->Primitive_type = GEO_LINE;
          o->AddDoubleAttribute("DRVAL2", v.num);
          assoc_chart.assoc.push_back(o);
        }
      }
    }

    LUPrec *lup = plib.S52_LUPLookup(tname, objname.c_str(), &obj, false);

    std::string out = "{\"id\":" + id_json + ",\"obj\":";
    mj::esc(out, objname);
    out += ",\"prim\":";
    mj::esc(out, prim);

    if (!lup) {
      out += ",\"lup\":null,\"rules\":[],\"expanded\":[]}";
      std::cout << out << "\n";
      continue;
    }
    nresolved++;

    plib._LUP2rules(lup, &obj);

    std::string inst(lup->INST.ToUTF8());
    out += ",\"lup\":{\"rcid\":" + std::to_string(lup->RCID);
    out += ",\"tnam\":\"" + std::string(lupname_str(lup->TNAM)) + "\"";
    out += ",\"dpri\":" + std::to_string((int)lup->DPRI - (int)PRIO_NODATA);
    out += ",\"rpri\":\"" + std::string(1, (char)lup->RPRI) + "\"";
    out += ",\"disc\":\"" + std::string(discat_str(lup->DISC)) + "\"";
    out += ",\"lucm\":" + std::to_string(lup->LUCM);
    out += ",\"inst\":";
    mj::esc(out, inst);
    out += "}";

    // Raw rule list, straight off the LUP.
    std::vector<std::string> raw = split_instructions(inst);
    out += ",\"rules\":" + mj::strlist(raw);

    // Expanded stream: every CS() replaced by what the procedure returns.
    ObjRazRules rz;
    memset(&rz, 0, sizeof(rz));
    rz.obj = &obj;
    rz.LUP = lup;

    std::vector<std::string> expanded;
    std::vector<std::string> cs_missing;
    std::vector<std::string> cs_called;
    std::vector<std::string> work = raw;
    for (int depth = 0; depth < 4 && !work.empty(); depth++) {
      std::vector<std::string> next_round;
      bool any_cs = false;
      for (const std::string &tok : work) {
        if (tok.compare(0, 3, "CS(") == 0) {
          any_cs = true;
          std::string name = tok.substr(3, 8);
          std::string result;
          cs_called.push_back(name);
          if (call_cs(name, &rz, result)) {
            for (const std::string &r : split_instructions(result))
              next_round.push_back(r);
          } else {
            cs_missing.push_back(name);
            next_round.push_back(tok);
          }
        } else {
          next_round.push_back(tok);
        }
      }
      if (!any_cs) break;
      work = next_round;
      // Anything still holding a CS() goes round again; the depth cap stops
      // a procedure that returns its own name from looping forever.
      bool more = false;
      for (const std::string &t : work)
        if (t.compare(0, 3, "CS(") == 0 && cs_missing.empty()) more = true;
      if (!more) break;
    }
    expanded = work;

    // Visibility, from OpenCPN's own ObjectRenderCheckCat: display category,
    // SCAMIN and SUPER_SCAMIN in one verdict.
    if (opt.view_scale > 0) {
      const mj::Value *cs = rec.find("chart_scale");
      ctx.chart_scale = cs ? (int)cs->num : 0;
      obj.m_DisplayCat = lup->DISC;
      bool visible = plib.ObjectRenderCheckCat(&rz);
      out += std::string(",\"visible\":") + (visible ? "true" : "false");
    }

    out += ",\"expanded\":" + mj::strlist(expanded);
    // Which CS procedures ran. The differ uses this to flag results that
    // depend on chart context the oracle does not have (associated depth
    // areas, floating/rigid ATON lists), so those are not read as navcore bugs.
    if (!cs_called.empty()) out += ",\"cs\":" + mj::strlist(cs_called);
    if (assoc_given) out += ",\"assoc_used\":true";
    if (!cs_missing.empty()) out += ",\"cs_missing\":" + mj::strlist(cs_missing);
    out += "}";
    std::cout << out << "\n";
  }

  std::cout.flush();
  fprintf(stderr, "s52oracle: %ld features in, %ld resolved to a LUP\n", nread, nresolved);
  return 0;
}
