// Link-time stubs for the parts of s52plib the oracle never executes.
//
// s52plib is one translation unit for both symbology resolution and
// rasterisation. The oracle only calls the former, so the rendering half
// (tessellation, texture fonts, canvas blitting) is satisfied here. Every stub
// that must never run aborts loudly rather than returning a plausible lie — if
// the oracle ever reaches one, the run fails instead of emitting a wrong
// answer.

#include <wx/wx.h>
#include <wx/font.h>

#include <cstdio>
#include <cstdlib>

#include "s52s57.h"
#include "mygeom.h"
#include "TexFont.h"
#include "DepthFont.h"
#include "vector2D.h"

[[noreturn]] static void unreachable(const char *what) {
  fprintf(stderr, "s52oracle: FATAL: rendering path '%s' reached in oracle\n", what);
  abort();
}

// ---- OpenCPN application glue -------------------------------------------

// Set from main(): the directory *containing* s57data/, used by the plib to
// find s57objectclasses.csv.
wxString g_SharedDataLocation;

extern "C" wxString *GetpSharedDataLocation() { return &g_SharedDataLocation; }

extern "C" bool GetGlobalColor(wxString, wxColour *pcolour) {
  // Only used for UI chrome (text foregrounds); symbology colours come from
  // the plib's own colour tables.
  if (pcolour) *pcolour = wxColour(0, 0, 0);
  return true;
}

wxColour GetFontColour_PlugIn(wxString) { return wxColour(0, 0, 0); }

wxFont *FindOrCreateFont_PlugIn(int point_size, wxFontFamily family,
                                wxFontStyle style, wxFontWeight weight,
                                bool underline, const wxString &facename,
                                wxFontEncoding encoding) {
  static wxFont *f = nullptr;
  if (!f)
    f = new wxFont(point_size, family, style, weight, underline, facename, encoding);
  return f;
}

wxFont *GetOCPNScaledFont_PlugIn(wxString, int default_size) {
  static wxFont *f = nullptr;
  if (!f)
    f = new wxFont(default_size ? default_size : 10, wxFONTFAMILY_SWISS,
                   wxFONTSTYLE_NORMAL, wxFONTWEIGHT_NORMAL);
  return f;
}

float GetOCPNChartScaleFactor_Plugin() { return 1.0f; }

// ---- geometry / rasterisation -------------------------------------------

int PolyTessGeo::BuildDeferredTess(void) { unreachable("PolyTessGeo::BuildDeferredTess"); }

render_canvas_parms::render_canvas_parms(void) { unreachable("render_canvas_parms"); }
render_canvas_parms::~render_canvas_parms(void) {}

extern "C" double vGetLengthOfNormal(pVector2D, pVector2D, pVector2D) {
  unreachable("vGetLengthOfNormal");
}

// ---- texture fonts -------------------------------------------------------

TexFont::~TexFont() {}
void TexFont::Delete() {}

DepthFont::DepthFont() {}
DepthFont::~DepthFont() {}
void DepthFont::Build(wxFont *, double, double) { unreachable("DepthFont::Build"); }
void DepthFont::Delete() {}
bool DepthFont::GetGLTextureRect(wxRect &, int) { unreachable("DepthFont::GetGLTextureRect"); }
