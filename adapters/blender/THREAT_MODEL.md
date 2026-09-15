# Threat model

Blender scripts can execute arbitrary code, so the adapter generates scripts only from a closed operation schema and validates resource paths before launch. The model cannot supply a Python source string.

