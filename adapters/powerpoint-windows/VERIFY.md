# Verification contract

Open verifies the bound presentation `FullName` equals the requested path (`application_state`). Slide create/delete verify the slide count moved by exactly one; reorder verifies the moved `SlideID` sits at the destination index; shape text set verifies an immediate `TextRange` readback equals the requested string (`application_state`). Save verifies the backing file exists with nonzero size and reports before/after sha256; PDF export verifies the emitted file exists with nonzero size and sha256 (`persisted_artifact`). A successful COM call alone is never proof.
