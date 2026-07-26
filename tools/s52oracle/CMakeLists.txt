cmake_minimum_required(VERSION 3.16)
project(s52oracle CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Root of OpenCPN's vendored libraries (libs/s52plib, libs/geoprim, libs/pugixml).
set(OCPN_LIBS "${CMAKE_CURRENT_SOURCE_DIR}/../../doc/openCPN/libs"
    CACHE PATH "OpenCPN libs/ directory")

find_package(wxWidgets REQUIRED COMPONENTS core base)
include(${wxWidgets_USE_FILE})

add_executable(s52oracle
  src/main.cpp
  ${OCPN_LIBS}/s52plib/src/s52plib.cpp
  ${OCPN_LIBS}/s52plib/src/s52cnsy.cpp
  ${OCPN_LIBS}/s52plib/src/chartsymbols.cpp
  ${OCPN_LIBS}/s52plib/src/s52utils.cpp
  ${OCPN_LIBS}/geoprim/src/bbox.cpp
  ${OCPN_LIBS}/geoprim/src/line_clip.cpp
  ${OCPN_LIBS}/geoprim/src/poly_math.cpp
  ${OCPN_LIBS}/geoprim/src/LOD_reduce.cpp
  ${OCPN_LIBS}/pugixml/pugixml.cpp
  src/stubs.cpp
)

target_include_directories(s52oracle PRIVATE
  src
  ${OCPN_LIBS}/s52plib/src
  ${OCPN_LIBS}/geoprim/src
  ${OCPN_LIBS}/pugixml
)

target_link_libraries(s52oracle PRIVATE ${wxWidgets_LIBRARIES})

# The rendering half of s52plib (symbol rasterisation, tessellation, texture
# fonts) is never called by the oracle; stubs.cpp satisfies the linker.
target_compile_options(s52oracle PRIVATE -Wno-deprecated-declarations)
