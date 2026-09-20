# Threat model

COM automation can drive the live PowerPoint process, so the adapter binds presentations only by normalized full path, refuses ambiguous multi-deck requests instead of guessing `ActivePresentation`, and scopes file paths under `COMPTROL_PRESENTATIONS_ROOT` when set. Macro/VBA execution is never offered: payload keys requesting macros are refused, and the adapter never calls `Application.Run` or touches the VB project. Saves and PDF exports are verified by file readback (existence plus nonzero size and sha256) rather than trusting the COM call return.
