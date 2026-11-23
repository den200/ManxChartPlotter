# Testing Manx

A test suite is only worth its count if it fails when the code is wrong. In
October 2026 Manx's 565 unit tests let 29 of 32 deliberately planted S-52 bugs
through, and its OpenCPN oracle had silently stopped running two months
earlier while 2,008 features drifted from OpenCPN. Every layer below is
therefore built to answer one question with a machine, not an opinion: *would
this have failed if the code were wrong?*

## The layers

| layer | what it checks | run | where |
|---|---|---|---|
| unit tests | logic, function by function, against spec values | `cargo test` | everywhere, CI |
| property tests | invariants over random inputs (round-trips, conservation, never-panic) | part of `cargo test` | everywhere, CI |
| OpenCPN oracles | S-52 symbology and S-57 reading, feature by feature | `tools/conformance.sh` | locally |
| golden images | what actually reaches the screen | `tools/golden.sh` | locally |
| stress | the suite passes in any time zone, thread count and HOME | `tools/test-stress.sh` | locally |

### Unit tests

Expected values come from the specification (S-52, S-57, IHO), from OpenCPN's
behaviour, from physics or from a published reference — never from "whatever
the code returns now". A test that pins current output pins current bugs: two
did exactly that, one asserting a safety contour no line carried, one listing
SYMINS01 as unimplemented in OpenCPN.

Tests must not touch the real user's settings, cache or charts (use
`tempfile`), must not depend on the network or the wall clock, and must not
depend on environment variables a developer may have exported.

### Mutation testing (retired)

In October 2026 [cargo-mutants](https://mutants.rs) was run over the
navigation, weather, routing and S-52 code: it plants one mechanical bug at a
time and reports the ones no test notices. It turned up about 25 real bugs,
all fixed with their tests. It is no longer run: a whole-crate run takes hours
and saturated the development Mac to the point of crashing it, and the
changed-lines CI job took three hours a pull request. The lesson stays: a
test's expected value must be one a broken version of the code would miss.

### OpenCPN oracles

`tools/conformance.sh` runs both, and exits non-zero on failure:

- **S-52** (`tools/s52oracle`, `tools/s52diff.py`): every feature of the
  233 NOAA Massachusetts cells resolved by Manx and by OpenCPN's s52plib.
  (Not the o-charts set: the check dumps features to files, which
  o-charts' conditions do not allow for their charts.) Fails on any
  divergence not listed as a known deviation in `tools/s52oracle/README.md`,
  and when the oracle itself fails to run.
- **S-57** (`tools/s57oracle`, `tools/s57diff.py`): all 233 NOAA
  Massachusetts cells, updates applied. Fails on any differing feature,
  attribute or geometry.

Needs the OpenCPN sources under `doc/` and the licensed charts; neither is in
the repository. Run it when anything under `src/s52`, `src/senc`, `src/s57`
or `assets/s52` changes.

### Golden images

`tools/golden.sh` captures fixed views — Copenhagen harbour in day, dusk and
night, the Sound with two safety-contour settings and the standard display,
the Great Belt, a tilted view, a route, and a public-domain Boston harbour
cell (`tests/fixtures/s57-harbour`) — and compares each with its reference.
Captures are pixel-identical run to run, so the tolerance only absorbs a
driver update: a channel must move by more than 4 levels, and more than
0.05 % of the pixels must change. A failure writes a diff image with the
changed pixels in magenta.

A golden image guards what its views show and nothing else; a colour used
nowhere in them can change unseen. It also freezes what is drawn without
judging it: the references were checked by eye when they were made.
After a deliberate rendering change, look at the diffs, then re-bless with
`tools/golden.sh --update`. Every view is of the public-domain NOAA fixture cell and is
committed. There are no views of o-charts: their conditions forbid keeping
a rendering of the charts, and `MANX_SHOT` refuses to capture one.

### Stress

`tools/test-stress.sh` runs the unit tests 20 times, alternating one thread
and many, across UTC, Copenhagen, Chatham (+12:45) and St John's (−3:30), with
HOME pointing at an empty folder. A test that fails here leans on something
other than the code.

## Definition of done

A change is finished when:

1. `cargo test` passes;
2. if it touches symbology or chart reading: `tools/conformance.sh` passes;
3. if it touches rendering: `tools/golden.sh` passes, or its diffs were
   looked at and the references re-blessed in the same change.
