#!/usr/bin/env python3
from pathlib import Path

path = Path(__file__).resolve().parents[1] / "crates/comptrol-core/src/adapters.rs"
text = path.read_text()

old_ppt = '''                    capabilities: vec![
                        "presentation.desktop.open".to_owned(),
                        "presentation.slide.create".to_owned(),
                        "presentation.slide.delete".to_owned(),
                        "presentation.slide.reorder".to_owned(),
                        "presentation.shape.text.set".to_owned(),
                        "presentation.save".to_owned(),
                        "presentation.export_pdf".to_owned(),
                    ],
'''
new_ppt = '''                    capabilities: vec![
                        "presentation.desktop.open".to_owned(),
                        "presentation.desktop.batch_edit".to_owned(),
                        "presentation.slide.create".to_owned(),
                        "presentation.slide.delete".to_owned(),
                        "presentation.slide.reorder".to_owned(),
                        "presentation.slide.duplicate".to_owned(),
                        "presentation.shape.text.set".to_owned(),
                        "presentation.shape.textbox.create".to_owned(),
                        "presentation.shape.image.insert".to_owned(),
                        "presentation.shape.delete".to_owned(),
                        "presentation.shape.geometry.set".to_owned(),
                        "presentation.save".to_owned(),
                        "presentation.export_pdf".to_owned(),
                    ],
'''
if text.count(old_ppt) != 1:
    raise SystemExit(f"PowerPoint catalog block count={text.count(old_ppt)}")
text = text.replace(old_ppt, new_ppt, 1)

old_canva = '''                        "design.element.group".to_owned(),
                        "design.export".to_owned(),
'''
new_canva = '''                        "design.element.group".to_owned(),
                        "design.batch_edit".to_owned(),
                        "design.export".to_owned(),
'''
if text.count(old_canva) != 1:
    raise SystemExit(f"Canva catalog block count={text.count(old_canva)}")
text = text.replace(old_canva, new_canva, 1)

path.write_text(text)
print("adapter runtime catalog synchronized")
