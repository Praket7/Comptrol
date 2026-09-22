# PowerPoint adapter (offline Open XML)

Offline adapter over the `.pptx` Open XML format using python-pptx. It reads slide text and notes, applies closed `presentation.batch_edit` operations for text, images, notes, slide structure, basic vector/text shapes, shape positioning/rotation, and fill/line/font colors, and exports `.pptx` copies or PDFs via a locally probed `soffice` converter. Every mutation checkpoints the source to `.bak` first and reopens the result for readback verification. Macros/VBA are never executed. PowerPoint animation effects, embedded audio/video editing, SmartArt, and arbitrary Office UI features are outside this adapter's current scope.

Install the Python dependency into Comptrol's local adapter environment with `~/.comptrol/venv/bin/python -m pip install -r adapters/powerpoint/requirements.txt` after creating the venv.
