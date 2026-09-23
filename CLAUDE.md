# Claude Code Rules for NavCore

## MANDATORY: Read Before Implementing

Before writing ANY code, you MUST:

1. **Read the relevant doc first** and quote the specific section you're implementing
2. **Use exact versions** from `doc/navcore_plan_v2.md` - no substitutions
3. **Follow patterns** from `doc/wgpu ref notes.md` for all WGPU code
4. **Ask if unsure** - don't guess or use "similar" APIs

## Key Documents

- `doc/navcore_plan_v2.md` - Master implementation plan with dependency versions
- `doc/wgpu ref notes.md` - WGPU API patterns to follow exactly
- `doc/SENC_RENDER_BLUEPRINT.md` - SENC file format details
- `doc/pi5-test-rig.md` - the Raspberry Pi 5 test machine: `ssh rpi5`, deploy, running on its screen

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

## KISS Principle

This project follows KISS (Keep It Simple, Stupid). Don't add:
- Features not in the current milestone
- Dependencies not in the plan
- Abstractions "for later"
