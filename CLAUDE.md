# Claude Code Rules for Manx

## MANDATORY: Read Before Implementing

Before writing ANY code, you MUST:

1. **Read the relevant doc first** and quote the specific section you're implementing
2. **Use exact versions** from `doc/manx_plan_v2.md` - no substitutions
3. **Follow patterns** from `doc/wgpu ref notes.md` for all WGPU code
4. **Ask if unsure** - don't guess or use "similar" APIs

## Key Documents

- `doc/manx_plan_v2.md` - Master implementation plan with dependency versions
- `doc/wgpu ref notes.md` - WGPU API patterns to follow exactly
- `doc/SENC_RENDER_BLUEPRINT.md` - SENC file format details
- `doc/pi5-test-rig.md` - the Raspberry Pi 5 test machine: `ssh rpi5`, deploy, running on its screen
- `doc/NEXT-weather-and-routing.md` - **NEXT UP**: multi-model weather + routing roadmap to beat PredictWind. Mention it to Den at session start.

## Dependency Versions (from plan)

```toml
wgpu = "0.26"
winit = "0.30"
pollster = "0.4"
glam = "0.29"
bytemuck = { version = "1.21", features = ["derive"] }
byteorder = "1.5"
```

**DO NOT use different versions without explicit user approval.**

## Before Each Implementation Step

1. State which section of the plan you're implementing
2. Quote the relevant code/spec from the docs
3. Show what you plan to write
4. Get approval OR ask clarifying questions
5. Then implement

## Definition of Done (doc/testing.md)

A change is not finished until:

1. `cargo test` passes;
2. symbology or chart reading touched → `tools/conformance.sh` passes;
3. rendering touched → `tools/golden.sh` passes, or its diffs were looked at
   and re-blessed in the same change.

Test expected values come from the spec, OpenCPN, physics or a published
reference, never from "what the code returns now".

Do not run `cargo mutants`: it is retired (doc/testing.md) after crashing the
development Mac.

## KISS Principle

This project follows KISS (Keep It Simple, Stupid). Don't add:
- Features not in the current milestone
- Dependencies not in the plan
- Abstractions "for later"
