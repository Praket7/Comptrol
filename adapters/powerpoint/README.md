# PowerPoint adapter (offline Open XML)

Offline adapter over the `.pptx` Open XML format using python-pptx. It reads slide text and notes, applies closed `presentation.batch_edit` ops (replace_text, insert_image, speaker_notes.set, slide.create/delete/reorder), and exports clean `.pptx` copies or PDFs via a locally probed `soffice` converter. Every mutation checkpoints the source to `.bak` first and reopens the result for readback verification. Macros/VBA are never executed.
