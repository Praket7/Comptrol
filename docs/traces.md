# Traces and replay

Comptrol can record operation requests and structured results as local JSON lines.

Privacy minimal mode redacts typed values and removes postconditions. Developer mode keeps request structure while redacting typed values. Fixture full mode preserves synthetic fixture values for deterministic replay.

Record a trace with `comptrol record INPUT TRACE fixture_full`. Use `-` as INPUT to read JSON lines from standard input.

Replay requires `COMPTROL_REPLAY_FIXTURE=1`. Replay accepts only readiness, observation, and sandbox file fixture intents. It prints expected and actual results and stops on the first divergence.

Replay never grants permission for real desktop or browser mutation.

