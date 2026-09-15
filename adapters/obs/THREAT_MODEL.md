# Threat model

OBS control can affect external broadcasts. The adapter therefore separates local recording risk from streaming risk and never stores the OBS password. The host supplies it only through the user-configured process environment when an approved connection is made.

