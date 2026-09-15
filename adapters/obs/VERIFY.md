# Verification contract

Every scene or source mutation is followed by a GetCurrentScene or GetInputSettings query. Recording status is verified through GetRecordStatus after start or stop. A successful WebSocket response alone is delivery evidence, not task completion.

