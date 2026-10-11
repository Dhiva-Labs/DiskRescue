# Specifications

Research notes that pin down the on-disk formats, wire formats, and
algorithms DiskRescue implements. Each file is self-contained: exact byte
offsets, tables, pseudocode, edge cases, a fixture strategy for tests, and
the primary sources it was checked against. Implementers treat these as the
reference; anything marked UNVERIFIED must be confirmed against the source
before relying on it.
