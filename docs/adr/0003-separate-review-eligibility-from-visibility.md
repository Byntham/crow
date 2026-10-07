> Amended by [ADR 0011](0011-review-versions-pushed-by-listed-authors.md): with an author list, Crow reviews a version automatically only when a listed author pushed it.

# Select reviews by author rather than repository visibility

Crow supports explicitly enrolled public and private GitHub repositories. Each repository defaults to reviewing only the operator's PRs and can instead allow selected authors or everyone. A private-only restriction would exclude the operator's intended use, so repository visibility cannot substitute for author eligibility or controls over access to the review machine.

Eligibility follows the target repository's author policy for both same-repository and fork PRs. The source fork does not need to be separately enrolled, and being a fork is not itself a reason to skip a PR.
