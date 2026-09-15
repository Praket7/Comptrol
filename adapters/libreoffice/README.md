# LibreOffice adapter

The adapter talks to the actual LibreOffice process through its UNO API. It never replaces a document with a fixture implementation. Start LibreOffice with a user-approved local UNO listener and run `python3 src/adapter.py` as the isolated host child.

