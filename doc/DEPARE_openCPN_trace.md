# OpenCPN DEPARE Rendering Trace (SENC -> LUP -> Fill Color)

This file shows the **actual OpenCPN code path** for DEPARE depth areas, from SENC ingestion to LUP selection and final area fill, using the OpenCPN sources embedded in this repo.

---

## 1) SENC ingestion → S57Obj vector (S57 chart)

**File:** `doc/reference projects/OpenCPN/gui/src/s57chart.cpp`

```cpp
int s57chart::BuildRAZFromSENCFile(const wxString &FullPath) {
  int ret_val = 0;  // default is OK

  Osenc sencfile;

  // Set up the containers for ingestion results.
  // These will be populated by Osenc, and owned by the caller (this).
  S57ObjVector Objects;
  VE_ElementVector VEs;
  VC_ElementVector VCs;

  sencfile.setRefLocn(ref_lat, ref_lon);

  int srv = sencfile.ingest200(FullPath, &Objects, &VEs, &VCs);

  if (srv != SENC_NO_ERROR) {
    wxLogMessage(sencfile.getLastError());
    // TODO  Clean up here, or massive leaks result
    return 1;
  }

  //  Get the cell Ref point as recorded in the SENC
  Extent ext = sencfile.getReadExtent();

  m_FullExtent.ELON = ext.ELON;
  m_FullExtent.WLON = ext.WLON;
  m_FullExtent.NLAT = ext.NLAT;
  m_FullExtent.SLAT = ext.SLAT;
  m_bExtentSet = true;

  ref_lat = (ext.NLAT + ext.SLAT) / 2.;
  ref_lon = (ext.ELON + ext.WLON) / 2.;

  // ... edge table handling omitted ...

  // Walk the vector of S57Objs, associating LUPS, instructions, etc...

  for (unsigned int i = 0; i < Objects.size(); i++) {
    S57Obj *obj = Objects[i];

    //      This is where Simplified or Paper-Type point features are selected
    LUPrec *LUP;
    LUPname LUP_Name = PAPER_CHART;

    const wxString objnam = obj->GetAttrValueAsString("OBJNAM");
    if (objnam.Len() > 0) {
      const wxString fe_name = wxString(obj->FeatureName, wxConvUTF8);
      SendVectorChartObjectInfo(FullPath, fe_name, objnam, obj->m_lat,
                                obj->m_lon, scale, nativescale);
    }
    // If there is a localized object name and it actually is different from the
    // object name, send it as well...
    const wxString nobjnam = obj->GetAttrValueAsString("NOBJNM");
    if (nobjnam.Len() > 0 && nobjnam != objnam) {
      const wxString fe_name = wxString(obj->FeatureName, wxConvUTF8);
      SendVectorChartObjectInfo(FullPath, fe_name, nobjnam, obj->m_lat,
                                obj->m_lon, scale, nativescale);
    }

    switch (obj->Primitive_type) {
      case GEO_POINT:
      case GEO_META:
      case GEO_PRIM:

        if (PAPER_CHART == ps52plib->m_nSymbolStyle)
          LUP_Name = PAPER_CHART;
        else
          LUP_Name = SIMPLIFIED;

        break;

      case GEO_LINE:
        LUP_Name = LINES;
        break;

      case GEO_AREA:
        if (PLAIN_BOUNDARIES == ps52plib->m_nBoundaryStyle)
          LUP_Name = PLAIN_BOUNDARIES;
        else
          LUP_Name = SYMBOLIZED_BOUNDARIES;

        break;
    }

    LUP = ps52plib->S52_LUPLookup(LUP_Name, obj->FeatureName, obj);

    if (NULL == LUP) {
      if (g_bDebugS57) {
        wxString msg(obj->FeatureName, wxConvUTF8);
        msg.Prepend(_T("   Could not find LUP for "));
        LogMessageOnce(msg);
      }
      delete obj;
      obj = NULL;
      Objects[i] = NULL;
    } else {
      //              Convert LUP to rules set
      ps52plib->_LUP2rules(LUP, obj);

      //              Add linked object/LUP to the working set
      _insertRules(obj, LUP, this);

      //              Establish Object's Display Category
      obj->m_DisplayCat = LUP->DISC;

      //              Establish objects base display priority
      obj->m_DPRI = LUP->DPRI - '0';

      //  Is this a category-movable object?
      if (!strncmp(obj->FeatureName, "OBSTRN", 6) ||
          !strncmp(obj->FeatureName, "WRECKS", 6) ||
          !strncmp(obj->FeatureName, "DEPCNT", 6) ||
          !strncmp(obj->FeatureName, "UWTROC", 6)) {
        obj->m_bcategory_mutable = true;
      } else {
        obj->m_bcategory_mutable = false;
      }
    }

    // ... ATON handling omitted ...

  }  // Objects iterator

  // ... chart metadata ...

  ObjRazRules *top;

  AssembleLineGeometry();

  return ret_val;
}
```

---

## 2) S57Obj structure (attributes + geometry slots)

**File:** `doc/reference projects/OpenCPN/libs/s52plib/src/s52s57.h`

```cpp
class S57Obj {
public:
  //  Public Methods
  S57Obj();
  ~S57Obj();

  S57Obj(const char *featureName);

  wxString GetAttrValueAsString(const char *attr);
  int GetAttributeIndex(const char *AttrSeek);

  bool AddIntegerAttribute(const char *acronym, int val);
  bool AddIntegerListAttribute(const char *acronym, int *pval, int nValue);
  bool AddDoubleAttribute(const char *acronym, double val);
  bool AddDoubleListAttribute(const char *acronym, double *pval, int nValue);
  bool AddStringAttribute(const char *acronym, char *val);

  bool SetPointGeometry(double lat, double lon, double ref_lat, double ref_lon);
  bool SetLineGeometry(LineGeometryDescriptor *pGeo, GeoPrim_t geoType,
                       double ref_lat, double ref_lon);
  bool SetAreaGeometry(PolyTessGeo *ppg, double ref_lat, double ref_lon);
  bool SetMultipointGeometry(MultipointGeometryDescriptor *pGeo, double ref_lat,
                             double ref_lon);

  // Private Methods
private:
  void Init();

public:
  // Instance Data
  char FeatureName[8];
  GeoPrim_t Primitive_type;

  char *att_array;
  wxArrayOfS57attVal *attVal;
  int n_attr;

  int iOBJL;
  int Index;

  double x;  // for POINT
  double y;
  double z;
  int npt;  // number of points as needed by arrays

  pt *geoPt;  // used for cm93 line feature select check

  double *geoPtz;      // an array[3] for MultiPoint, SM with Z, i.e. depth
  double *geoPtMulti;  // an array[2] for MultiPoint, lat/lon to make bbox
                       // of decomposed points
  PolyTessGeo *pPolyTessGeo;

  LLBBox BBObj;  // lat/lon BBox of the rendered object
  double m_lat;  // The lat/lon of the object's "reference" point
  double m_lon;

  Rules *CSrules;  // per object conditional symbology
  int bCS_Added;

  S52_TextC *FText;
  int bFText_Added;
  wxRect rText;

  int Scamin;  // SCAMIN attribute decoded during load
  int SuperScamin;
  bool bIsClone;
  int nRef;            // Reference counter, to signal OK for deletion
  bool bIsAton;        // This object is an aid-to-navigation
  bool bIsAssociable;  // This object is DRGARE or DEPARE

  int m_n_lsindex;
  int *m_lsindex_array;
  int m_n_edge_max_points;
  line_segment_element *m_ls_list;
  PI_line_segment_element *m_ls_list_legacy;

  DisCat m_DisplayCat;
  int m_DPRI;                // display priority, assigned from initial LUP
                             // May be adjusted by CS
  bool m_bcategory_mutable;  //  CS procedure may move this object to a higher
                             //  category. Used as a hint to rendering filter
                             //  logic

  // This transform converts from object geometry
  // to SM coordinates.
  double x_rate;    // These auxiliary transform coefficients are
  double y_rate;    // to be used in GetPointPix() and friends
  double x_origin;  // on a per-object basis if necessary
  double y_origin;

  chart_context *m_chart_context;  // per-chart constants, carried in each
                                   // object for convenience
  int auxParm0;  // some per-object auxiliary parameters, used for OpenGL
  int auxParm1;
  int auxParm2;
  int auxParm3;

  bool bBBObj_valid;
};
```

---

## 3) DEPARE LUP selection (chartsymbols.xml)

**File:** `doc/reference projects/OpenCPN/data/s57data/chartsymbols.xml`

```xml
<lookup id="39" RCID="32075" name="DEPARE">
    <type>Area</type>
    <disp-prio>Group 1</disp-prio>
    <radar-prio>Suppressed</radar-prio>
    <table-name>Plain</table-name>
    <attrib-code index="0">DRVAL1?</attrib-code>
    <attrib-code index="1">DRVAL2?</attrib-code>
    <instruction>AC(NODTA);AP(PRTSUR01);LS(SOLD,2,CHGRD)</instruction>
    <display-cat>Displaybase</display-cat>
    <comment>13030</comment>
</lookup>
<lookup id="40" RCID="32076" name="DEPARE">
    <type>Area</type>
    <disp-prio>Group 1</disp-prio>
    <radar-prio>Suppressed</radar-prio>
    <table-name>Plain</table-name>
    <instruction>CS(DEPARE01)</instruction>
    <display-cat>Displaybase</display-cat>
    <comment>13030</comment>
</lookup>
```

---

## 4) LUP lookup logic (S52_LUPLookup + FindBestLUP)

**File:** `doc/reference projects/OpenCPN/libs/s52plib/src/s52plib.cpp`

```cpp
LUPrec *s52plib::S52_LUPLookup(LUPname LUP_Name, const char *objectName,
                               S57Obj *pObj, bool bStrict) {
  LUPrec *LUP = NULL;

  LUPArrayContainer *plac = SelectLUPArrayContainer(LUP_Name);

  LUPHashIndex *hip = plac->GetArrayIndexHelper(objectName);
  int nLUPs = hip->count;
  int nStartIndex = hip->n_start;

  LUP = FindBestLUP(plac->GetLUPArray(), nStartIndex, nLUPs, pObj, bStrict);

  return LUP;
}
```

```cpp
LUPrec *s52plib::FindBestLUP(wxArrayOfLUPrec *LUPArray, unsigned int startIndex,
                             unsigned int count, S57Obj *pObj, bool bStrict) {
  //  Check the parameters
  if (0 == count) return NULL;
  if (startIndex >= LUPArray->GetCount()) return NULL;

  // setup default return to the first LUP that matches Feature name.
  LUPrec *LUP = LUPArray->Item(startIndex);

  int nATTMatch = 0;
  int countATT = 0;
  bool bmatch_found = false;

  if (pObj->att_array == NULL)
    goto check_LUP;  // object has no attributes to compare, so return "best"
                     // LUP

  for (unsigned int i = 0; i < count; ++i) {
    LUPrec *LUPCandidate = LUPArray->Item(startIndex + i);

    if (!LUPCandidate->ATTArray.size())
      continue;  // this LUP has no attributes coded

    countATT = 0;
    char *currATT = pObj->att_array;
    int attIdx = 0;

    for (unsigned int iLUPAtt = 0; iLUPAtt < LUPCandidate->ATTArray.size();
         iLUPAtt++) {
      // Get the LUP attribute name
      const char *slatc = LUPCandidate->ATTArray[iLUPAtt].c_str();

      if (slatc && (strlen(slatc) < 6))
        goto next_LUP_Attr;  // LUP attribute value not UTF8 convertible (never
                             // seen in PLIB 3.x)

      if (slatc) {
        const char *slatv = slatc + 6;
        while (attIdx < pObj->n_attr) {
          if (0 == strncmp(slatc, currATT, 6)) {
            // OK we have an attribute name match

            bool attValMatch = false;

            // special case (i)
            if (!strncmp(slatv, " ", 1)) {  // any object value will match wild
                                            // card (S52 para 8.3.3.4)
              ++countATT;
              goto next_LUP_Attr;
            }

            // special case (ii)
            // TODO  Find an ENC with "UNKNOWN" DRVAL1 or DRVAL2 and debug this
            // code
            if (!strncmp(slatv, "?",
                         1)) {  // if LUP attribute value is "undefined"

              //  Match if the object does NOT contain this attribute
              goto next_LUP_Attr;
            }

            // checking against object attribute value
            S57attVal *v = (pObj->attVal->Item(attIdx));

            switch (v->valType) {
              case OGR_INT:  // S57 attribute type 'E' enumerated, 'I' integer
              {
                int LUP_att_val = atoi(slatv);
                if (LUP_att_val == *(int *)(v->value)) attValMatch = true;
                break;
              }

              case OGR_INT_LST:  // S57 attribute type 'L' list: comma separated
                                 // integer
              {
                int a;
                char ss[41];
                strncpy(ss, slatv, 39);
                ss[40] = '\0';
                char *s = &ss[0];

                int *b = (int *)v->value;
                sscanf(s, "%d", &a);

                while (*s != '\0') {
                  if (a == *b) {
                    sscanf(++s, "%d", &a);
                    b++;
                    attValMatch = true;

                  } else
                    attValMatch = false;
                }
                break;
              }
              case OGR_REAL:  // S57 attribute type'F' float
              {
                double obj_val = *(double *)(v->value);
                float att_val = atof(slatv);
                if (fabs(obj_val - att_val) < 1e-6)
                  if (obj_val == att_val) attValMatch = true;
                break;
              }

              case OGR_STR:  // S57 attribute type'A' code string, 'S' free text
              {
                //    Strings must be exact match
                //    n.b. OGR_STR is used for S-57 attribute type 'L',
                //    comma-separated list

                // wxString cs( (char *) v->value, wxConvUTF8 ); // Attribute
                // from object if( LATTC.Mid( 6 ) == cs )
                if (!strcmp((char *)v->value, slatv)) attValMatch = true;
                break;
              }

              default:
                break;
            }  // switch

            // value match
            if (attValMatch) ++countATT;

            goto next_LUP_Attr;
          }  // if attribute name match

          //  Advance to the next S57obj attribute
          currATT += 6;
          ++attIdx;

        }  // while
      }    // if

    next_LUP_Attr:

      currATT = pObj->att_array;  // restart the object attribute list
      attIdx = 0;
    }  // for iLUPAtt

    //      Create a "match score", defined as fraction of candidate LUP
    //      attributes actually matched by feature. Used later for resolving
    //      "ties"

    int nattr_matching_on_candidate = countATT;
    int nattrs_on_candidate = LUPCandidate->ATTArray.size();
    double candidate_score =
        (1. * nattr_matching_on_candidate) / (1. * nattrs_on_candidate);

    //       According to S52 specs, match must be perfect,
    //         and the first 100% match is selected
    if (candidate_score == 1.0) {
      LUP = LUPCandidate;
      bmatch_found = true;
      break;  // selects the first 100% match
    }

  }  // for loop

check_LUP:
  //  In strict mode, we require at least one attribute to match exactly

  if (bStrict) {
    if (nATTMatch == 0)  // nothing matched
      LUP = NULL;
  } else {
    //      If no match found, return the first LUP in the list which has no
    //      attributes
    if (!bmatch_found) {
      for (unsigned int j = 0; j < count; ++j) {
        LUPrec *LUPtmp = NULL;

        LUPtmp = LUPArray->Item(startIndex + j);
        if (!LUPtmp->ATTArray.size()) {
          return LUPtmp;
        }
      }
    }
  }

  return LUP;
}
```

---

## 5) LUP → Rules (StringToRules / _LUP2rules)

**File:** `doc/reference projects/OpenCPN/libs/s52plib/src/s52plib.cpp`

```cpp
int s52plib::_LUP2rules(LUPrec *LUP, S57Obj *pObj) {
  if (NULL == LUP) return -1;
  // check if already parsed
  if (LUP->ruleList != NULL) {
    return 0;
  }

  if (!LUP->INST.IsEmpty()) {
    Rules *top = StringToRules(LUP->INST);
    LUP->ruleList = top;

    return 1;
  } else
    return 0;
}
```

---

## 6) Conditional Symbology (DEPARE01 dispatch)

**Cond table entry**

**File:** `doc/reference projects/OpenCPN/libs/s52plib/src/s52cnsy.cpp`

```cpp
Cond condTable[] = {
    {"CLRLIN01", CLRLIN01},
    {"DATCVR01", DATCVR01},
    {"DATCVR01", DATCVR01},
    {"DEPARE01", DEPARE01},
    {"DEPARE02", DEPARE01},  // new in PLIB 3_3, opencpn defaults to DEPARE01
    {"DEPCNT02", DEPCNT02},
    {"DEPVAL01", DEPVAL01},
    {"LEGLIN02", LEGLIN02},
    {"LIGHTS05", LIGHTS05},  // new in PLIB 3_3, replaces LIGHTS04
    {"LITDSN01", LITDSN01},
    {"OBSTRN04", OBSTRN04},
    {"OWNSHP02", OWNSHP02},
    {"PASTRK01", PASTRK01},
    {"QUAPOS01", QUAPOS01},
    {"QUALIN01", QUALIN01},
    {"QUAPNT01", QUAPNT01},
    {"SLCONS03", SLCONS03},
    {"RESARE02", RESARE02},
    {"RESTRN01", RESTRN01},
    //   {"RESCSP01",RESCSP01},
    {"SEABED01", SEABED01},
    // ...
};
```

**CS execution**

**File:** `doc/reference projects/OpenCPN/libs/s52plib/src/s52plib.cpp`

```cpp
char *s52plib::RenderCS(ObjRazRules *rzRules, Rules *rules) {
  void *ret;
  void *(*f)(void *);

  static int f05;

  if (rules->razRule == NULL) {
    if (!f05)
      //                  CPLError ( ( CPLErr ) 0, 0,"S52plib:_renderCS(): ERROR
      //                  no conditional symbology for: %s\n", rules->INSTstr );
      f05++;
    return 0;
  }

  void *g = (void *)rules->razRule;

  f = (void *(*)(void *))g;
  ret = f((void *)rzRules);

  return (char *)ret;
}
```

```cpp
void s52plib::GetAndAddCSRules(ObjRazRules *rzRules, Rules *rules) {
  LUPrec *NewLUP;
  LUPrec *LUP;
  LUPrec *LUPCandidate;
  wxString cs_string;

  char *rule_str1 = RenderCS(rzRules, rules);
  if (rule_str1)
    cs_string = wxString(rule_str1, wxConvUTF8);
  free(rule_str1);  // delete rule_str1;

  //  Try to find a match for this object/attribute set in dynamic CS LUP Table

  //  Do this by checking each LUP in the CS LUPARRAY and checking....
  //  a) is Object Name the same? and
  //  b) was LUP created earlier by exactly the same INSTruction string?
  //  c) does LUP have same Display Category and Priority?

  wxArrayOfLUPrec *la = condSymbolLUPArray;
  int index = 0;
  int index_max = la->GetCount();
  LUP = NULL;

  while ((index < index_max)) {
    LUPCandidate = la->Item(index);
    if (!strcmp(rzRules->LUP->OBCL, LUPCandidate->OBCL)) {
      if (LUPCandidate->INST.IsSameAs(cs_string)) {
        if (LUPCandidate->DISC == rzRules->LUP->DISC) {
          LUP = LUPCandidate;
          break;
        }
      }
    }
    index++;
  }

  //  If not found, need to create a dynamic LUP and add to CS LUP Table

  if (NULL == LUP)  // Not found
  {
    NewLUP = new LUPrec();
    NewLUP->DISC = rzRules->LUP->DISC;  // as a default

    memcpy(NewLUP->OBCL, rzRules->LUP->OBCL, 6);  // the object class name

    //      Add the complete CS string to the LUP
    if(cs_string.Length())
      NewLUP->INST = cs_string;

    _LUP2rules(NewLUP, rzRules->obj);

    // Add LUP to array
    wxArrayOfLUPrec *pLUPARRAYtyped = condSymbolLUPArray;

    pLUPARRAYtyped->Add(NewLUP);

    LUP = NewLUP;

  }  // if (LUP = NULL)

  Rules *top = LUP->ruleList;

  rzRules->obj->CSrules = top;  // patch in a new rule set
}
```

---

## 7) DEPARE01 conditional symbology (chooses AC color token)

**File:** `doc/reference projects/OpenCPN/libs/s52plib/src/s52cnsy.cpp`

```cpp
static void *DEPARE01(void *param) {
  ObjRazRules *rzRules = (ObjRazRules *)param;
  S57Obj *obj = rzRules->obj;

  double drval1, drval2;
  bool drval1_found;

  //      Determine the color based on mariner selections

  drval1 = -1.0;  // default values
  drval1_found = GetDoubleAttr(obj, "DRVAL1", drval1);
  // drval2 = drval1 + 0.01;
  GetDoubleAttr(obj, "DRVAL2", drval2);
  //make sure drval2 is higher then drval1, always even in bad charts
  if (drval2 <= drval1) drval2 = drval1 + 0.01;

  //   Create a string of the proper color reference

  wxString rule_str = _T("AC(DEPIT)");

  if (drval1 >= 0.0 && drval2 > 0.0) rule_str = _T("AC(DEPVS)");

  if (TRUE == S52_getMarinerParam(S52_MAR_TWO_SHADES)) {
    if (drval1 >= S52_getMarinerParam(S52_MAR_SAFETY_CONTOUR) &&
        drval2 > S52_getMarinerParam(S52_MAR_SAFETY_CONTOUR)) {
      rule_str = _T("AC(DEPDW)");
    }
  } else {
    if (drval1 >= S52_getMarinerParam(S52_MAR_SHALLOW_CONTOUR) &&
        drval2 > S52_getMarinerParam(S52_MAR_SHALLOW_CONTOUR))
      rule_str = _T("AC(DEPMS)");

    if (drval1 >= S52_getMarinerParam(S52_MAR_SAFETY_CONTOUR) &&
        drval2 > S52_getMarinerParam(S52_MAR_SAFETY_CONTOUR)) {
      rule_str = _T("AC(DEPMD)");
    }

    if (drval1 >= S52_getMarinerParam(S52_MAR_DEEP_CONTOUR) &&
        drval2 > S52_getMarinerParam(S52_MAR_DEEP_CONTOUR)) {
      rule_str = _T("AC(DEPDW)");
    }
  }

  //  If object is DRGARE....

  if (!strncmp(rzRules->LUP->OBCL, "DRGARE", 6)) {
    if (!drval1_found)  // If DRVAL1 was not defined...
    {
      rule_str = _T("AC(DEPMD)");
    }
    rule_str.Append(_T(";AP(DRGARE01)"));
    rule_str.Append(_T(";LS(DASH,1,CHGRF)"));

    // Todo Restrictions
    /*
            char pval[30];
            if(true == GetStringAttr(obj, "RESTRN", pval, 20))
            {
                GString *rescsp01 = _RESCSP01(geo);
                if (NULL != rescsp01)
                {
                    g_string_append(depare01, rescsp01->str);
                    g_string_free(rescsp01, TRUE);
                }
            }
    */
  }

  rule_str.Append('\037');

  return strdup(rule_str.mb_str());
}
```

---

## 8) Area rendering (AC fill → getColor → polygon fill)

**File:** `doc/reference projects/OpenCPN/libs/s52plib/src/s52plib.cpp`

```cpp
int s52plib::RenderAreaToDC(wxDC *pdcin, ObjRazRules *rzRules,
                            render_canvas_parms *pb_spec) {
  if (!ObjectRenderCheckRules(rzRules, true)) return 0;

  m_pdc = pdcin;  // use this DC
  Rules *rules = rzRules->LUP->ruleList;

  while (rules != NULL) {
    switch (rules->ruleType) {
      case RUL_ARE_CO:
        RenderToBufferAC(rzRules, rules, pb_spec);
        break;  // AC
      case RUL_ARE_PA:
        RenderToBufferAP(rzRules, rules, pb_spec);
        break;  // AP

      case RUL_CND_SY: {
        if (!rzRules->obj->bCS_Added) {
          rzRules->obj->CSrules = NULL;
          GetAndAddCSRules(rzRules, rules);
          rzRules->obj->bCS_Added = 1;  // mark the object
        }
        Rules *rules_last = rules;
        rules = rzRules->obj->CSrules;

        //    The CS procedure may have changed the Display Category of the
        //    Object, need to check again for visibility
        if (ObjectRenderCheckCat(rzRules)) {
          while (NULL != rules) {
            // Hve seen drgare fault here, need to code area query to debug
            // possible that RENDERtoBUFFERAP/AC is blowing obj->CSRules
            //    When it faults here, look at new debug field obj->CSLUP
            switch (rules->ruleType) {
              case RUL_ARE_CO:
                RenderToBufferAC(rzRules, rules, pb_spec);
                break;
              case RUL_ARE_PA:
                RenderToBufferAP(rzRules, rules, pb_spec);
                break;
```

```cpp
int s52plib::RenderToBufferAC(ObjRazRules *rzRules, Rules *rules,
                              render_canvas_parms *pb_spec) {
  //if (vp->m_projection_type != PROJECTION_MERCATOR) return 1;

  S52color *c;
  char *str = (char *)rules->INSTstr;

  c = getColor(str);

  RenderToBufferFilledPolygon(rzRules, rzRules->obj, c, pb_spec, NULL);

  //    At very small scales, the object could be visible on both the left and
  //    right sides of the screen. Identify this case......
  if (vp_plib.chart_scale > 5e7) {
    //    Does the object hang out over the left side of the VP?
    if ((rzRules->obj->BBObj.GetMaxLon() > GetBBox().GetMinLon()) &&
        (rzRules->obj->BBObj.GetMinLon() < GetBBox().GetMinLon())) {
      //    If we add 360 to the objects lons, does it intersect the the right
      //    side of the VP?
      if (((rzRules->obj->BBObj.GetMaxLon() + 360.) >
           GetBBox().GetMaxLon()) &&
          ((rzRules->obj->BBObj.GetMinLon() + 360.) <
           GetBBox().GetMaxLon())) {
        //  If so, this area oject should be drawn again, this time for the left
        //  side
        //    Do this by temporarily adjusting the objects rendering offset
        rzRules->obj->x_origin -=
            mercator_k0 * WGS84_semimajor_axis_meters * 2.0 * PI;
        RenderToBufferFilledPolygon(rzRules, rzRules->obj, c, pb_spec, NULL);
        rzRules->obj->x_origin +=
            mercator_k0 * WGS84_semimajor_axis_meters * 2.0 * PI;
      }
    }
  }

  return 1;
}
```

---

## 9) Color token lookup (chart symbols color table)

**File:** `doc/reference projects/OpenCPN/libs/s52plib/src/s52plib.cpp`

```cpp
S52color *s52plib::getColor(const char *colorName) {
  S52color *c;
  c = m_chartSymbols.GetColor(colorName, m_colortable_index);
  return c;
}
```

---

## 10) Summary of DEPARE path in OpenCPN (by actual code order)

The complete path is constructed from the code shown above:

1) `Osenc::ingest200()` loads SENC and produces `S57Obj` instances.
2) `s57chart::BuildRAZFromSENCFile()` selects LUP set based on geometry and calls `S52_LUPLookup()`.  
3) `s52plib::FindBestLUP()` matches LUP attributes vs `S57Obj` attributes and picks the best LUP.  
4) `s52plib::_LUP2rules()` parses `LUP->INST` into `Rules` (AC/AP/CS/LS/etc).  
5) `RenderAreaToDC()` processes the rule list; for `CS(DEPARE01)` it calls `GetAndAddCSRules()` → `RenderCS()` → `DEPARE01()` which returns a new instruction string like `AC(DEPVS)`/`AC(DEPMD)`/`AC(DEPDW)` etc.  
6) `RenderToBufferAC()` calls `getColor(token)` and fills the area via `RenderToBufferFilledPolygon()`.

Everything above is copied directly from OpenCPN source in this repo.
