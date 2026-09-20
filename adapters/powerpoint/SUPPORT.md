# Support boundary

Supported: `presentation.read`, `presentation.batch_edit` (closed op schema only), and `presentation.export` to `pdf`/`pptx` on Windows, macOS, and Linux when python-pptx is importable. Image assets must resolve under `COMPTROL_PRESENTATIONS_ROOT` when that root is set. PDF export requires a local `soffice`/`libreoffice` binary (or `COMPTROL_SOFFICE_BIN`); otherwise it is refused honestly. Unsupported: macro/VBA execution, `.pptm` macro preservation, arbitrary file writes outside the declared scope, and in-place edits of `.pptm` sources.
