> Superseded by [ADR 0009](0009-run-opt-in-isolated-experiments.md). Runtime testing is available by default and remains under local worker control.

# Limit initial reviews to code inspection

Crow's first release reviews changes through file reads, code searches, and Git diff inspection. It does not install repository dependencies, execute repository-provided scripts or tests, edit files, or push fixes. This gives up runtime verification in exchange for a simpler execution boundary on the operator's persistent machine; adding code execution would require a separate design decision.
