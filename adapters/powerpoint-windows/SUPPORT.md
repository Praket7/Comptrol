# Support boundary

Supported on Windows only: `presentation.desktop.open`, `presentation.slide.create/delete/reorder`, `presentation.shape.text.set`, `presentation.save`, and `presentation.export_pdf` through the PowerPoint COM object model with comtypes installed. Requests must identify the deck by `presentation_path`; blind `ActivePresentation` use is refused unless exactly one presentation is open. Unsupported: VBA/macro execution, non-Windows hosts, missing comtypes (reported as `unsupported`), and paths outside `COMPTROL_PRESENTATIONS_ROOT` when that root is set.
