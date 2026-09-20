# PowerPoint Windows adapter (COM)

Deep desktop adapter for Microsoft PowerPoint on Windows via comtypes (`PowerPoint.Application`). It opens decks by exact full path, creates/deletes/reorders slides, sets shape text, saves, and exports PDFs, binding the exact presentation every time and refusing ambiguous requests when more than one deck matches. VBA macros are never executed. On hosts without comtypes the handshake reports `unsupported` honestly.
