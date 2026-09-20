# Verification contract

Searches and reads are verified from live Mail readbacks, returning bounded excerpts rather than raw script output. Drafts are verified with a drafts-mailbox readback counting messages with the same subject. Sends are never verified by osascript success: verified=true additionally requires a sent-mailbox readback observing a message with the same subject; anything less returns verified=false with an `apple_mail_delivery_only_unverified` marker and an explicit reason. File bytes never travel through the frame.

Verify with: `python3 -m py_compile src/adapter.py`, a handshake frame through stdio expecting `comptrol.apple-mail`, a payload carrying a `script` key asserting `arbitrary_script_refused`, an attachment outside `COMPTROL_MAIL_ASSETS_ROOT` asserting `attachment_outside_scope`, and (on macOS with a test account) a draft plus a send asserting the drafts/sent readback counts before accepting verified=true.
