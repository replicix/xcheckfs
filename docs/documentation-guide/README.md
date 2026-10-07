# Documentation guide

## Where should my document go?

The docs folder structure for this project is the following, with
explanations of what belongs in each section:

- **documentation-guide**: meta information about writing documentation
  for this repository
- **explanation**: understanding-oriented discussion — decisions,
  architecture rationale, goals and non-goals, why something exists
- **how-to-guides**: goal-oriented directions addressing a specific goal
  or problem (including developer procedures such as testing)
- **reference**: technical description of how specific things within this
  project work (CLI surfaces, formats, protocols, limitations)
- **tutorials**: learning-oriented experiences to get you up to speed
- **plans**: sequenced implementation plans under `plans/vX/{wip,done}`,
  with companion roadmap and progress logs per version

## Rules of thumb

- Document every feature and every limitation exactly once, in the page
  that owns it, and link to it from everywhere else it matters.
- Ground statements in the code. When behavior changes, change the page in
  the same commit.
- Keep pages short. If a page needs a table of contents, consider splitting it.

## Documentation templates

Templates for this repository include:

- [Feature documentation template](./feature-documentation-template.md)
