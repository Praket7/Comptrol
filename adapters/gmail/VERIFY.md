# Verification contract

Reads and searches are verified from live users.messages.get responses, returning bounded excerpts plus attachment metadata rather than raw API JSON. Drafts are verified with a drafts.get readback matching the created draft and message ids. Sends are never verified by HTTP acceptance: verified=true additionally requires a Sent-folder readback (users.messages.get on the returned id) whose thread id matches and whose labelIds include SENT; anything less returns verified=false with a `gmail_accepted_unverified` marker. File bytes never travel through the frame.

Verify with: `python3 -m py_compile src/adapter.py`, a handshake frame through stdio expecting `comptrol.gmail`, a search against a loopback stub via `COMPTROL_GMAIL_API_BASE` asserting bounded results, a send asserting verified=false when the stub omits the SENT label, and a send asserting verified=true with matching message id, thread id, and SENT membership when the readback confirms it.
