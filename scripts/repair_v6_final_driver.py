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
path.write_text(text[:start] + replacement + text[end:], encoding="utf-8")
print("repaired V6 final driver")
