from pathlib import Path

path = Path("scripts/apply_v6_final.py")
text = path.read_text(encoding="utf-8")
start = text.index("# Teach the verified-control skill to use safe read fan-out and promoted workflows.")
end = text.index("# Lightweight regression tests for the new safety boundaries.", start)
replacement = '''# Teach the verified-control skill to use safe read fan-out and promoted workflows.
skill_path = "plugins/comptrol/skills/comptrol-verified-control/SKILL.md"
skill = read(skill_path)
skill_marker = "\\n\\nIf the local server is not running"
if skill.count(skill_marker) != 1:
    raise RuntimeError(f"skill insertion marker: expected one match, found {skill.count(skill_marker)}")
skill_guidance = (
    "\\n- For independent preflight observations, use `workflow.execute` with `parallel_reads` (maximum 8); the runtime only accepts explicitly allowlisted R0 reads and never speculates mutations."
    "\\n- Promote a repeated workflow only with clean-fixture, independently verified replay evidence. Reuse it by `promoted_workflow_id` and include its fingerprint when available; fall back to a cold verified route on any mismatch."
)
skill = skill.replace(skill_marker, skill_guidance + skill_marker, 1)
write(skill_path, skill)

'''
text = text[:start] + replacement + text[end:]

# The legacy persistence regression asserted the old global route key. V6 route
# statistics are intentionally contextual, so accept the contextual native key
# while retaining compatibility with stores produced by the previous version.
final_write = 'write(core_path, core)\nprint("V6 final execution pass applied")\n'
if text.count(final_write) != 1:
    raise RuntimeError(f"final core write marker: expected one match, found {text.count(final_write)}")
test_patch = '''legacy_lookup = '.and_then(|routes| routes.iter().find(|route| route["route"] == "native"))'
contextual_lookup = '.and_then(|routes| {\\n                routes.iter().find(|route| {\\n                    route["route"].as_str().is_some_and(|key| {\\n                        key == "native" || key.starts_with("native|")\\n                    })\\n                })\\n            })'
if core.count(legacy_lookup) != 1:
    raise RuntimeError(f"legacy route history lookup: expected one match, found {core.count(legacy_lookup)}")
core = core.replace(legacy_lookup, contextual_lookup, 1)

write(core_path, core)
print("V6 final execution pass applied")
'''
text = text.replace(final_write, test_patch, 1)
path.write_text(text, encoding="utf-8")
print("repaired V6 final driver")
