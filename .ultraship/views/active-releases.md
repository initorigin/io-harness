<!--
Generated from canonical UltraShip state. Do not edit directly.
Run `ultraship views` to regenerate.
-->
# Active releases

## io-harness 0.60.1

**Execution state:** DEVELOPING
**Release fit:** probable
**Target mode:** published
**Outcome:** A developer deciding whether to embed this crate can read the landing page once and come away knowing three things: what the harness does today, what the parts of it that cost anything actually cost on a named machine, and which guarantees it holds that the harness they would otherwise reach for does not.
**What the landing page is today.** 618 lines, of which 366 are one flat section — thirty-one bold-led paragraphs with no index, no ordering a reader can predict, and no summary above them. Every paragraph is accurate; density is not the problem. The problem is that most of them are written as the release that added them rather than as the capability they are: "Since 0.46.0 every command `exec` ... runs inside the sandbox", "**0.47.0 closed the Linux hole in this table**", "Through 0.48.0 a request held one system string", "Through 0.49.0 a child came back as `[child 7 ...]`", "**0.59.0 closed the Windows hole**". A reader who has never run this crate is being handed the diff between two releases they have never used, and has to reconstruct the present state from it. That is CHANGELOG.md's job, and CHANGELOG.md does it — 6,301 lines of it.
**After this release** the README is present tense throughout. It opens with what the crate is and a table of contents, states the capability matrix in one table before any prose, keeps the prose that carries real depth, links the five measurements this repository has taken with the machine named on each, and carries a comparison against the harnesses a reader is choosing between — re-verified this release against each project's current sources and dated in the table, so a reader can tell how old the claim is and check it.
**And no fact is lost.** Every release-anchored statement removed from the README lands in the release table `docs/CAPABILITIES.md` already carries — a file already keyed by release, so the project still has exactly one version history rather than a second one competing with the CHANGELOG.



_Canonical sources: products/<id>/execution/active.yaml, products/<id>/releases/<version>.yaml_
