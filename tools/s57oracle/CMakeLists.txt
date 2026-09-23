cmake_minimum_required(VERSION 3.16)
project(s57oracle CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Root of the OpenCPN source tree (libs/iso8211, libs/s57-charts, libs/gdal,
# data/s57data).
set(OCPN_ROOT "${CMAKE_CURRENT_SOURCE_DIR}/../../doc/reference projects/OpenCPN"
    CACHE PATH "OpenCPN source tree")
set(OCPN_LIBS "${OCPN_ROOT}/libs")

file(GLOB ISO8211_SRC "${OCPN_LIBS}/iso8211/src/*.cpp")
file(GLOB GDAL_SRC "${OCPN_LIBS}/gdal/src/*.cpp")

# s57registrar_mgr.cpp is left out: it is wx-based and only maps acronyms to
# SENC attribute ids, which the oracle does not need.
add_executable(s57oracle
  src/main.cpp
  ${ISO8211_SRC}
  ${GDAL_SRC}
  ${OCPN_LIBS}/s57-charts/src/ogrs57datasource.cpp
  ${OCPN_LIBS}/s57-charts/src/ogrs57layer.cpp
  ${OCPN_LIBS}/s57-charts/src/s57classregistrar.cpp
  ${OCPN_LIBS}/s57-charts/src/s57featuredefns.cpp
  ${OCPN_LIBS}/s57-charts/src/s57reader.cpp
)

# s57-charts/include first: libs/iso8211/include carries an older copy of s57.h.
target_include_directories(s57oracle PRIVATE
  ${OCPN_LIBS}/s57-charts/include
  ${OCPN_LIBS}/iso8211/include
  ${OCPN_LIBS}/gdal/include
  ${OCPN_LIBS}/gdal/include/gdal
)

target_compile_definitions(s57oracle PRIVATE
  OCPN_S57DATA="${OCPN_ROOT}/data/s57data")

# 1999-era GDAL code: silence its warnings, keep ours.
set_source_files_properties(${ISO8211_SRC} ${GDAL_SRC}
  ${OCPN_LIBS}/s57-charts/src/ogrs57datasource.cpp
  ${OCPN_LIBS}/s57-charts/src/ogrs57layer.cpp
  ${OCPN_LIBS}/s57-charts/src/s57classregistrar.cpp
  ${OCPN_LIBS}/s57-charts/src/s57featuredefns.cpp
  ${OCPN_LIBS}/s57-charts/src/s57reader.cpp
  PROPERTIES COMPILE_OPTIONS "-w")
