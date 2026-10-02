# Linux dependency refresh (OPT-2035)

Source review baseline: `2bd3a81ac0f0d4896e30521721ea7b2b5b4e4219`, 2 October 2026.
Linear project P-OPT-63, repository PORTALSURFER/yield. This snapshot covers all
170 issues initially labeled Linux: Blocked and their 87 additional reachable
prerequisites. Full descriptions and latest comments were read; relations were
refreshed after reconciliation. The graph had no cycles. This is dispatch evidence,
not acceptance of these issues. Live Linear state and current source supersede it.

Removed 62 completed prerequisite links across 27 open issues; removed another
seven remaining links after OPT-1642 merged PR #196. Overlap means 30 distinct open
issues were updated. Four independently executable audits/decisions/guards are now
ready. Five ready contract leaves OPT-2031..2034 and OPT-2036 name previously implicit
portable blockers. None supplies a production receipt or physical qualification.

See [the execution policy and five-agent lanes](LINUX_FIVE_AGENT_EXECUTION_PLAN.md).

## Remaining root requirements

Reach counts overlap: one issue can require several roots. A root contract decision
must lead to actual consumer implementation; it does not waive that implementation.
Tracking parents in this table are aggregate requirements to decompose into concrete
leaves, not commands to execute the whole parent or stop work.

| Root | Reach in this snapshot | Next action |
| --- | ---: | --- |
| [OPT-1632](https://linear.app/boostnlvp/issue/OPT-1632) | 107 | Separate portable owner/admission work from accepted physical primitive and workflow qualification; retained Unsupported is not success. |
| [OPT-1633](https://linear.app/boostnlvp/issue/OPT-1633) | 70 | Separate portable owner/admission work from accepted physical primitive and workflow qualification; retained Unsupported is not success. |
| [OPT-2031](https://linear.app/boostnlvp/issue/OPT-2031) | 68 | Claim and execute its stated portable scope; then refresh consumer dependencies. |
| [OPT-2032](https://linear.app/boostnlvp/issue/OPT-2032) | 53 | Claim and execute its stated portable scope; then refresh consumer dependencies. |
| [OPT-1655](https://linear.app/boostnlvp/issue/OPT-1655) | 43 | Claim and execute its stated portable scope; then refresh consumer dependencies. |
| [OPT-2033](https://linear.app/boostnlvp/issue/OPT-2033) | 31 | Claim and execute its stated portable scope; then refresh consumer dependencies. |
| [OPT-2011](https://linear.app/boostnlvp/issue/OPT-2011) | 29 | Separate portable owner/admission work from accepted physical primitive and workflow qualification; retained Unsupported is not success. |
| [OPT-2034](https://linear.app/boostnlvp/issue/OPT-2034) | 7 | Claim and execute its stated portable scope; then refresh consumer dependencies. |
| [OPT-1868](https://linear.app/boostnlvp/issue/OPT-1868) | 6 | Separate portable owner/admission work from accepted physical primitive and workflow qualification; retained Unsupported is not success. |
| [OPT-1626](https://linear.app/boostnlvp/issue/OPT-1626) | 5 | Identify the concrete platform/resource requirement; continue any independent portable implementation. |
| [OPT-1357](https://linear.app/boostnlvp/issue/OPT-1357) | 5 | Read child requirements; replace overbroad links with concrete unsatisfied leaf prerequisites. |
| [OPT-1351](https://linear.app/boostnlvp/issue/OPT-1351) | 5 | Read child requirements; replace overbroad links with concrete unsatisfied leaf prerequisites. |
| [OPT-1350](https://linear.app/boostnlvp/issue/OPT-1350) | 5 | Read child requirements; replace overbroad links with concrete unsatisfied leaf prerequisites. |
| [OPT-1339](https://linear.app/boostnlvp/issue/OPT-1339) | 5 | Read child requirements; replace overbroad links with concrete unsatisfied leaf prerequisites. |
| [OPT-1761](https://linear.app/boostnlvp/issue/OPT-1761) | 5 | Identify the concrete platform/resource requirement; continue any independent portable implementation. |
| [OPT-1489](https://linear.app/boostnlvp/issue/OPT-1489) | 5 | Read child requirements; replace overbroad links with concrete unsatisfied leaf prerequisites. |
| [OPT-2010](https://linear.app/boostnlvp/issue/OPT-2010) | 3 | Separate portable owner/admission work from accepted physical primitive and workflow qualification; retained Unsupported is not success. |
| [OPT-1663](https://linear.app/boostnlvp/issue/OPT-1663) | 2 | Claim and execute its stated portable scope; then refresh consumer dependencies. |
| [OPT-1661](https://linear.app/boostnlvp/issue/OPT-1661) | 2 | Claim and execute its stated portable scope; then refresh consumer dependencies. |
| [OPT-1636](https://linear.app/boostnlvp/issue/OPT-1636) | 2 | Claim and execute its stated portable scope; then refresh consumer dependencies. |
| [OPT-1243](https://linear.app/boostnlvp/issue/OPT-1243) | 2 | Read child requirements; replace overbroad links with concrete unsatisfied leaf prerequisites. |
| [OPT-2036](https://linear.app/boostnlvp/issue/OPT-2036) | 1 | Claim and execute its stated portable scope; then refresh consumer dependencies. |
| [OPT-1828](https://linear.app/boostnlvp/issue/OPT-1828) | 1 | Read child requirements; replace overbroad links with concrete unsatisfied leaf prerequisites. |

## Per-issue open prerequisites

Completed prerequisites are excluded. Ready indicates a verified independently
executable scope, not completed work. Original physical/native/aggregate acceptance
remains in Linear. No-dependency issues still require their stated work; an empty
blocking list is not proof that an owner or backend already exists.

| Issue | Scope | Dispatch/status | Open blocking relations |
| --- | --- | --- | --- |
| [OPT-1569](https://linear.app/boostnlvp/issue/OPT-1569) | [1473-D] Freeze the GPUI durable-service contract (decision) | Backlog | OPT-1836 |
| [OPT-1571](https://linear.app/boostnlvp/issue/OPT-1571) | [1479-E] Route one same-source move action | Backlog | OPT-1701, OPT-1632 |
| [OPT-1578](https://linear.app/boostnlvp/issue/OPT-1578) | [1479-A] Route one duplicate action | Backlog | OPT-1694, OPT-1583 |
| [OPT-1579](https://linear.app/boostnlvp/issue/OPT-1579) | [1479-G] Route one trash action | Backlog | OPT-1649, OPT-1703 |
| [OPT-1580](https://linear.app/boostnlvp/issue/OPT-1580) | [1480-A] Add one nested-folder create action | Backlog | OPT-1692 |
| [OPT-1581](https://linear.app/boostnlvp/issue/OPT-1581) | [1480-B] Add one folder rename action | Backlog | OPT-1700 |
| [OPT-1583](https://linear.app/boostnlvp/issue/OPT-1583) | [1473-A] Wire one create-command adapter | Backlog | OPT-1569 |
| [OPT-1584](https://linear.app/boostnlvp/issue/OPT-1584) | [1479-B] Route one copy action | Backlog | OPT-1693, OPT-1578 |
| [OPT-1585](https://linear.app/boostnlvp/issue/OPT-1585) | [1479-F] Route one cross-source move action | Backlog | OPT-1702, OPT-1643, OPT-1571 |
| [OPT-1586](https://linear.app/boostnlvp/issue/OPT-1586) | [1479-H] Route one permanent-delete action | Backlog | OPT-1652, OPT-1704, OPT-1579 |
| [OPT-1588](https://linear.app/boostnlvp/issue/OPT-1588) | [1480-C] Add one folder move action | Backlog | OPT-1701, OPT-1571 |
| [OPT-1589](https://linear.app/boostnlvp/issue/OPT-1589) | [1480-D] Add one folder delete action | Backlog | OPT-1650, OPT-1579 |
| [OPT-1590](https://linear.app/boostnlvp/issue/OPT-1590) | [1481-A] Route one fourth-negative trash action | Backlog | OPT-1703, OPT-1579 |
| [OPT-1591](https://linear.app/boostnlvp/issue/OPT-1591) | [1482-B] Remap one missing-source action | Backlog | OPT-1634 |
| [OPT-1592](https://linear.app/boostnlvp/issue/OPT-1592) | [1473-B] Wire one restore-command adapter | Backlog | OPT-1634, OPT-1632, OPT-1583 |
| [OPT-1593](https://linear.app/boostnlvp/issue/OPT-1593) | [1479-C] Route one import action | Backlog | OPT-1695, OPT-1584 |
| [OPT-1594](https://linear.app/boostnlvp/issue/OPT-1594) | [1479-I] Route one restore action | Backlog | OPT-1651, OPT-1667, OPT-1586, OPT-1579 |
| [OPT-1595](https://linear.app/boostnlvp/issue/OPT-1595) | [1479-K] Implement one mixed-batch conflict adapter | Backlog | OPT-1694, OPT-1578, OPT-1571 |
| [OPT-1596](https://linear.app/boostnlvp/issue/OPT-1596) | [1482-C] Fence one root-replacement action | Backlog | OPT-1591 |
| [OPT-1597](https://linear.app/boostnlvp/issue/OPT-1597) | [1473-C] Replay one crashed phase transition | Backlog | OPT-1667, OPT-1583, OPT-1592 |
| [OPT-1598](https://linear.app/boostnlvp/issue/OPT-1598) | [1475-B] Implement the shared prepared-edit dispatcher | Backlog | OPT-1698, OPT-1592 |
| [OPT-1599](https://linear.app/boostnlvp/issue/OPT-1599) | [1479-D] Route one rename/auto-rename action | Backlog | OPT-1700, OPT-1592 |
| [OPT-1600](https://linear.app/boostnlvp/issue/OPT-1600) | [1481-B] Route one Harvest copy action | Backlog | OPT-1693, OPT-1643, OPT-1645, OPT-1592, OPT-1584, OPT-1593 |
| [OPT-1601](https://linear.app/boostnlvp/issue/OPT-1601) | [1475-C] Dispatch trim | Backlog | OPT-1598 |
| [OPT-1602](https://linear.app/boostnlvp/issue/OPT-1602) | [1475-D] Dispatch crop | Backlog | OPT-1598 |
| [OPT-1603](https://linear.app/boostnlvp/issue/OPT-1603) | [1475-E] Dispatch silence-padded crop | Backlog | OPT-1598 |
| [OPT-1604](https://linear.app/boostnlvp/issue/OPT-1604) | [1475-F] Dispatch gain | Backlog | OPT-1598 |
| [OPT-1605](https://linear.app/boostnlvp/issue/OPT-1605) | [1475-G] Dispatch fade | Backlog | OPT-1598 |
| [OPT-1606](https://linear.app/boostnlvp/issue/OPT-1606) | [1475-H] Dispatch reverse | Backlog | OPT-1698, OPT-1598 |
| [OPT-1607](https://linear.app/boostnlvp/issue/OPT-1607) | [1475-I] Dispatch mute | Backlog | OPT-1598 |
| [OPT-1608](https://linear.app/boostnlvp/issue/OPT-1608) | [1475-J] Dispatch slide | Backlog | OPT-1598 |
| [OPT-1609](https://linear.app/boostnlvp/issue/OPT-1609) | [1476-C] Add one file-trash undo/recovery adapter | Backlog | OPT-1650, OPT-1597 |
| [OPT-1610](https://linear.app/boostnlvp/issue/OPT-1610) | [1477-A] Add one selection-extract adapter | Backlog | OPT-1697, OPT-1598, OPT-1583 |
| [OPT-1611](https://linear.app/boostnlvp/issue/OPT-1611) | [1476-A] Add committed-only waveform undo adapter | Backlog | OPT-1667, OPT-1601, OPT-1592 |
| [OPT-1612](https://linear.app/boostnlvp/issue/OPT-1612) | [1476-D] Add one folder-restore undo/recovery adapter | Backlog | OPT-1651, OPT-1667, OPT-1609 |
| [OPT-1613](https://linear.app/boostnlvp/issue/OPT-1613) | [1477-B] Add one extract-and-trim composite adapter | Backlog | OPT-1649, OPT-1610, OPT-1601 |
| [OPT-1614](https://linear.app/boostnlvp/issue/OPT-1614) | [1478-A] Create one selected-region artifact | Backlog | OPT-1655, OPT-1610 |
| [OPT-1615](https://linear.app/boostnlvp/issue/OPT-1615) | [1481-C] Recover one Harvest-copy cancellation | Backlog | OPT-1645, OPT-1600, OPT-1609 |
| [OPT-1616](https://linear.app/boostnlvp/issue/OPT-1616) | [1476-B] Restore one waveform bytes and selection/viewport action | Backlog | OPT-1601, OPT-1611 |
| [OPT-1617](https://linear.app/boostnlvp/issue/OPT-1617) | [1476-E] Add one collection-metadata undo adapter | Backlog | OPT-1611, OPT-1592 |
| [OPT-1618](https://linear.app/boostnlvp/issue/OPT-1618) | [1476-F] Map/register remaining waveform history variants | Backlog | OPT-1608, OPT-1607, OPT-1611, OPT-1602, OPT-1606, OPT-1604, OPT-1605, OPT-1603 |
| [OPT-1619](https://linear.app/boostnlvp/issue/OPT-1619) | [1476-G] Add one sample-metadata history adapter | Backlog | OPT-1611, OPT-1592 |
| [OPT-1620](https://linear.app/boostnlvp/issue/OPT-1620) | [1476-H] Add one file-relocation history adapter | Backlog | OPT-1701, OPT-1611, OPT-1571 |
| [OPT-1622](https://linear.app/boostnlvp/issue/OPT-1622) | [1478-B] Handoff one region through clipboard | Backlog | OPT-1655, OPT-1656, OPT-1592, OPT-1614 |
| [OPT-1623](https://linear.app/boostnlvp/issue/OPT-1623) | [1478-C] Handoff one region through native drag | Backlog | OPT-1656, OPT-1655, OPT-1592, OPT-1614 |
| [OPT-1624](https://linear.app/boostnlvp/issue/OPT-1624) | [1480-E] Add one folder restore action | Backlog | OPT-1667, OPT-1651, OPT-1612, OPT-1589 |
| [OPT-1625](https://linear.app/boostnlvp/issue/OPT-1625) | [1481-D] Undo one Harvest copy action | Backlog | OPT-1645, OPT-1611, OPT-1600, OPT-1615 |
| [OPT-1628](https://linear.app/boostnlvp/issue/OPT-1628) | [1480-F] Add one selected-folder batch conflict adapter | Backlog | OPT-1653, OPT-1580, OPT-1589, OPT-1591, OPT-1624, OPT-1588 |
| [OPT-1634](https://linear.app/boostnlvp/issue/OPT-1634) | [1339-E] Persist verified publication filesystem checkpoints | Backlog | OPT-1632 |
| [OPT-1635](https://linear.app/boostnlvp/issue/OPT-1635) | [1339-F] Persist cleanup and rollback filesystem checkpoints | Backlog | OPT-1633 |
| [OPT-1636](https://linear.app/boostnlvp/issue/OPT-1636) | [1339-G] Guard remaining direct filesystem side-effect paths | Ready | None; resolve stated scope |
| [OPT-1643](https://linear.app/boostnlvp/issue/OPT-1643) | [1341-B2] Persist idempotent cross-source reference participant command | Backlog | OPT-2032 |
| [OPT-1644](https://linear.app/boostnlvp/issue/OPT-1644) | [1341-B3] Persist idempotent operation-link participant command | Backlog | OPT-2033 |
| [OPT-1645](https://linear.app/boostnlvp/issue/OPT-1645) | [1341-C] Persist Harvest derivation and origin/destination checkpoints | Backlog | OPT-1643 |
| [OPT-1646](https://linear.app/boostnlvp/issue/OPT-1646) | [1341-D] Gate projection and readiness publication on checkpoints | Backlog | OPT-1787, OPT-1644, OPT-1645 |
| [OPT-1647](https://linear.app/boostnlvp/issue/OPT-1647) | [1341-E] Acknowledge matching watcher echoes after owned completion | Backlog | OPT-1645, OPT-1646 |
| [OPT-1648](https://linear.app/boostnlvp/issue/OPT-1648) | [1341-F] Replay one cross-boundary saga crash matrix | Backlog | OPT-1840, OPT-1647, OPT-1643, OPT-1645, OPT-1646, OPT-1644 |
| [OPT-1649](https://linear.app/boostnlvp/issue/OPT-1649) | [1342-A] Assign universal identity, claim, and disposition to folder recovery | Backlog | OPT-2011 |
| [OPT-1650](https://linear.app/boostnlvp/issue/OPT-1650) | [1342-B] Route folder staging and delete adapter through file owner | Backlog | OPT-1649, OPT-1634, OPT-1632 |
| [OPT-1651](https://linear.app/boostnlvp/issue/OPT-1651) | [1342-C] Route folder restore and collision-preserving restage adapter | Backlog | OPT-1649, OPT-1632, OPT-1634 |
| [OPT-1652](https://linear.app/boostnlvp/issue/OPT-1652) | [1342-D] Route folder purge and cleanup adapter | Backlog | OPT-1635, OPT-1633, OPT-1649 |
| [OPT-1653](https://linear.app/boostnlvp/issue/OPT-1653) | [1342-E] Gate folder source/global/projection publication | Backlog | OPT-1646, OPT-1649 |
| [OPT-1654](https://linear.app/boostnlvp/issue/OPT-1654) | [1342-F] Verify folder startup recovery and collision dispositions | Backlog | OPT-1652, OPT-1650, OPT-1653, OPT-1651 |
| [OPT-1655](https://linear.app/boostnlvp/issue/OPT-1655) | [1343-A] Define capability-bound ArtifactKey and store commands | Ready | None; resolve stated scope |
| [OPT-1656](https://linear.app/boostnlvp/issue/OPT-1656) | [1343-B] Track reverse ownership through source retirement | Backlog | OPT-1645, OPT-1655 |
| [OPT-1657](https://linear.app/boostnlvp/issue/OPT-1657) | [1343-C] Keep readiness independent from cache eviction/corruption | Backlog | OPT-1656, OPT-1655 |
| [OPT-1658](https://linear.app/boostnlvp/issue/OPT-1658) | [1343-D] Reconcile external artifact-directory mutations | Backlog | OPT-1656 |
| [OPT-1659](https://linear.app/boostnlvp/issue/OPT-1659) | [1343-E] Publish bounded artifact-store metrics | Backlog | OPT-1657, OPT-1658 |
| [OPT-1661](https://linear.app/boostnlvp/issue/OPT-1661) | [1344-B] Verify scanner, watcher, and directory-truth callers against source-writer routes | Ready | None; resolve stated scope |
| [OPT-1663](https://linear.app/boostnlvp/issue/OPT-1663) | [1344-D] Audit global, journal, and artifact caller exceptions against existing owners | Ready | None; resolve stated scope |
| [OPT-1664](https://linear.app/boostnlvp/issue/OPT-1664) | [1344-E] Enforce import direction and explicit fixture exceptions | Backlog | OPT-1661, OPT-1663, OPT-1636 |
| [OPT-1665](https://linear.app/boostnlvp/issue/OPT-1665) | [1345-A] Seed isolated journal and filesystem recovery fixtures | Backlog | OPT-1634, OPT-1635 |
| [OPT-1666](https://linear.app/boostnlvp/issue/OPT-1666) | [1345-B] Add isolated source/global/Harvest participant fixtures | Backlog | OPT-1644, OPT-1643, OPT-1645, OPT-1665 |
| [OPT-1667](https://linear.app/boostnlvp/issue/OPT-1667) | [1345-C] Run filesystem-boundary crash and replay matrix | Backlog | OPT-1635, OPT-1634, OPT-1665, OPT-1632, OPT-1633 |
| [OPT-1668](https://linear.app/boostnlvp/issue/OPT-1668) | [1345-D] Run source/global participant crash and replay matrix | Backlog | OPT-1644, OPT-1643, OPT-1645, OPT-1666, OPT-1648 |
| [OPT-1669](https://linear.app/boostnlvp/issue/OPT-1669) | [1345-E] Run derived projection/readiness/artifact/watcher replay matrix | Backlog | OPT-1647, OPT-1666, OPT-1657, OPT-1646 |
| [OPT-1670](https://linear.app/boostnlvp/issue/OPT-1670) | [1345-F] Verify startup admission and reverse-order shutdown | Backlog | OPT-1838, OPT-1666, OPT-1665 |
| [OPT-1671](https://linear.app/boostnlvp/issue/OPT-1671) | [1345-G] Persist machine-readable recovery evidence and status | Backlog | OPT-1667, OPT-1669, OPT-1668 |
| [OPT-1673](https://linear.app/boostnlvp/issue/OPT-1673) | [1346-B] Emit owner queue, writer wait, reserve, and retry metrics | Backlog | OPT-1670 |
| [OPT-1674](https://linear.app/boostnlvp/issue/OPT-1674) | [1346-C] Emit watcher continuity and proof-gap metrics | Backlog | OPT-1669 |
| [OPT-1675](https://linear.app/boostnlvp/issue/OPT-1675) | [1346-D] Emit projection, artifact, and stale-completion metrics | Backlog | OPT-1669, OPT-1658 |
| [OPT-1676](https://linear.app/boostnlvp/issue/OPT-1676) | [1346-E] Derive stable user status from durable disposition | Backlog | OPT-1671 |
| [OPT-1677](https://linear.app/boostnlvp/issue/OPT-1677) | [1346-F] Assemble reproducible SLO evidence from fixtures | Backlog | OPT-1673, OPT-1675, OPT-1676, OPT-1674 |
| [OPT-1678](https://linear.app/boostnlvp/issue/OPT-1678) | [1347-A] Certify operation-state and crash-recovery failure family | Backlog | OPT-1677, OPT-1667, OPT-1668 |
| [OPT-1679](https://linear.app/boostnlvp/issue/OPT-1679) | [1347-B] Certify watcher continuity and audit-gap failure family | Backlog | OPT-1674 |
| [OPT-1680](https://linear.app/boostnlvp/issue/OPT-1680) | [1347-C] Certify SQLite WAL/busy/reader failure family | Backlog | OPT-1677, OPT-1691 |
| [OPT-1681](https://linear.app/boostnlvp/issue/OPT-1681) | [1347-D] Certify capacity/cancellation/reserve failure family | Backlog | OPT-1821 |
| [OPT-1682](https://linear.app/boostnlvp/issue/OPT-1682) | [1347-E] Certify filesystem identity and collision failure family | Backlog | OPT-1635, OPT-1633, OPT-1632 |
| [OPT-1683](https://linear.app/boostnlvp/issue/OPT-1683) | [1347-F] Measure one reproducible large-source workload | Backlog | OPT-1677 |
| [OPT-1685](https://linear.app/boostnlvp/issue/OPT-1685) | [1348-A] Enforce WAL watermark admission states | Backlog | OPT-1836 |
| [OPT-1686](https://linear.app/boostnlvp/issue/OPT-1686) | [1348-B] Classify busy or locked as bounded scheduler retry | Backlog | OPT-1685 |
| [OPT-1687](https://linear.app/boostnlvp/issue/OPT-1687) | [1348-C] Implement owner-controlled checkpoint maintenance policy | Backlog | OPT-1686 |
| [OPT-1688](https://linear.app/boostnlvp/issue/OPT-1688) | [1348-D] Account WAL and SHM sidecars in capacity claims | Backlog | OPT-1687, OPT-1685 |
| [OPT-1689](https://linear.app/boostnlvp/issue/OPT-1689) | [1348-E] Bound read-only snapshots without hidden writes | Backlog | OPT-1687 |
| [OPT-1690](https://linear.app/boostnlvp/issue/OPT-1690) | [1348-F] Synchronize source remap snapshots with writer owner | Backlog | OPT-1689 |
| [OPT-1691](https://linear.app/boostnlvp/issue/OPT-1691) | [1348-G] Verify WAL fault dispositions and telemetry | Backlog | OPT-1687, OPT-1689, OPT-1690, OPT-1686, OPT-1685, OPT-1688 |
| [OPT-1692](https://linear.app/boostnlvp/issue/OPT-1692) | [1350-A] Admit create entrypoint before side effects | Backlog | OPT-2010 |
| [OPT-1693](https://linear.app/boostnlvp/issue/OPT-1693) | [1350-B] Admit copy entrypoint before side effects | Backlog | OPT-1634 |
| [OPT-1694](https://linear.app/boostnlvp/issue/OPT-1694) | [1350-C] Admit duplicate entrypoint before side effects | Backlog | OPT-1634 |
| [OPT-1695](https://linear.app/boostnlvp/issue/OPT-1695) | [1350-D] Admit import or drop entrypoint before side effects | Backlog | OPT-1634 |
| [OPT-1696](https://linear.app/boostnlvp/issue/OPT-1696) | [1350-E] Admit export entrypoint before side effects | Backlog | OPT-1634 |
| [OPT-1697](https://linear.app/boostnlvp/issue/OPT-1697) | [1350-F] Admit extraction entrypoint with bounded chunk claims | Backlog | OPT-1634, OPT-1633, OPT-1632 |
| [OPT-1698](https://linear.app/boostnlvp/issue/OPT-1698) | [1350-G] Admit rendered-edit entrypoint with bounded publication | Backlog | OPT-1632 |
| [OPT-1699](https://linear.app/boostnlvp/issue/OPT-1699) | [1350-H] Route history-restore adapter through durable admission | Backlog | OPT-2011, OPT-1633 |
| [OPT-1700](https://linear.app/boostnlvp/issue/OPT-1700) | [1351-A] Admit rename entrypoint before mutation | Backlog | OPT-2011 |
| [OPT-1701](https://linear.app/boostnlvp/issue/OPT-1701) | [1351-B] Admit same-source move entrypoint before mutation | Backlog | OPT-2011 |
| [OPT-1702](https://linear.app/boostnlvp/issue/OPT-1702) | [1351-C] Admit cross-source move before mutation | Backlog | OPT-1632, OPT-1633, OPT-1634 |
| [OPT-1703](https://linear.app/boostnlvp/issue/OPT-1703) | [1351-D] Admit trash entrypoint before staging | Backlog | OPT-1632 |
| [OPT-1704](https://linear.app/boostnlvp/issue/OPT-1704) | [1351-E] Admit permanent-delete entrypoint before mutation | Backlog | OPT-1633 |
| [OPT-1705](https://linear.app/boostnlvp/issue/OPT-1705) | [1351-F] Admit restore entrypoint with retained evidence | Backlog | OPT-1632 |
| [OPT-1706](https://linear.app/boostnlvp/issue/OPT-1706) | [1351-G] Admit purge entrypoint with cleanup capability | Backlog | OPT-1633 |
| [OPT-1707](https://linear.app/boostnlvp/issue/OPT-1707) | [1351-H] Route relocation undo adapter through durable admission | Backlog | OPT-2011 |
| [OPT-1708](https://linear.app/boostnlvp/issue/OPT-1708) | [1484-A] Add the production analysis job/readiness adapter | Backlog | OPT-1922 |
| [OPT-1709](https://linear.app/boostnlvp/issue/OPT-1709) | [1485-A] Add the GPUI starmap projection render adapter | Backlog | OPT-1708 |
| [OPT-1712](https://linear.app/boostnlvp/issue/OPT-1712) | [1488-A] Fence committed audio generations and invalidate cache | Backlog | OPT-1601 |
| [OPT-1718](https://linear.app/boostnlvp/issue/OPT-1718) | [1484-B] Reanalyze changed source and restart | Backlog | OPT-1243, OPT-1708 |
| [OPT-1719](https://linear.app/boostnlvp/issue/OPT-1719) | [1484-C] Add similarity anchor/aspect query stale fence | Backlog | OPT-1708 |
| [OPT-1720](https://linear.app/boostnlvp/issue/OPT-1720) | [1485-B] Add starmap pan/zoom and scaled hit targets | Backlog | OPT-1709 |
| [OPT-1721](https://linear.app/boostnlvp/issue/OPT-1721) | [1486-B] Persist one assignment tag rename | Backlog | OPT-2012 |
| [OPT-1722](https://linear.app/boostnlvp/issue/OPT-1722) | [1486-C] Persist one assignment tag delete | Backlog | OPT-2012 |
| [OPT-1723](https://linear.app/boostnlvp/issue/OPT-1723) | [1487-B] Add normalization durable command adapter | Backlog | OPT-1868, OPT-1598 |
| [OPT-1724](https://linear.app/boostnlvp/issue/OPT-1724) | [1488-B] Transform one crop/trim mark | Backlog | OPT-1712, OPT-1601 |
| [OPT-1725](https://linear.app/boostnlvp/issue/OPT-1725) | [1488-C] Transform one slide mark | Backlog | OPT-1712, OPT-1608 |
| [OPT-1726](https://linear.app/boostnlvp/issue/OPT-1726) | [1488-D] Restore meaningful undo state | Backlog | OPT-1712, OPT-1616 |
| [OPT-1727](https://linear.app/boostnlvp/issue/OPT-1727) | [1491-B] Run one large-library scan responsiveness workload | Backlog | OPT-1683 |
| [OPT-1728](https://linear.app/boostnlvp/issue/OPT-1728) | [1484-D] Render truthful analysis progress states | Backlog | OPT-1718, OPT-1708 |
| [OPT-1729](https://linear.app/boostnlvp/issue/OPT-1729) | [1485-C] Add starmap list/map selection stale fence | Backlog | OPT-1720, OPT-1709 |
| [OPT-1730](https://linear.app/boostnlvp/issue/OPT-1730) | [1485-E] Run one large-map frame-cost workload | Backlog | OPT-1709, OPT-1720 |
| [OPT-1731](https://linear.app/boostnlvp/issue/OPT-1731) | [1486-D] Reconcile one cross-source assignment failure | Backlog | OPT-1643, OPT-1721, OPT-1722 |
| [OPT-1732](https://linear.app/boostnlvp/issue/OPT-1732) | [1486-E] Converge dictionary chips and filters | Backlog | OPT-1721, OPT-1722 |
| [OPT-1733](https://linear.app/boostnlvp/issue/OPT-1733) | [1486-F] Add definitions and assignments undo adapter | Backlog | OPT-1668, OPT-1722, OPT-1721 |
| [OPT-1735](https://linear.app/boostnlvp/issue/OPT-1735) | [1488-E] Gate visual playback advance on rail acknowledgement | Backlog | OPT-1712, OPT-1726 |
| [OPT-1739](https://linear.app/boostnlvp/issue/OPT-1739) | [1491-C] Run one large-starmap responsiveness workload | Backlog | OPT-1730 |
| [OPT-1740](https://linear.app/boostnlvp/issue/OPT-1740) | [1491-D] Run one waveform repeated-edit workload | Backlog | OPT-1735 |
| [OPT-1745](https://linear.app/boostnlvp/issue/OPT-1745) | [1495-A] Build versioned profile migration fixture | Backlog | OPT-1666 |
| [OPT-1748](https://linear.app/boostnlvp/issue/OPT-1748) | [1491-F] Document responsiveness thresholds and CI/benchmark evidence | Backlog | OPT-1740, OPT-1727, OPT-1739 |
| [OPT-1770](https://linear.app/boostnlvp/issue/OPT-1770) | [1499-E] Evidence persistence/restart and stale-result parity | Backlog | OPT-1766 |
| [OPT-1773](https://linear.app/boostnlvp/issue/OPT-1773) | [1498-A] Match controlled release channel/version/platform fixtures | Backlog | OPT-1769 |
| [OPT-1775](https://linear.app/boostnlvp/issue/OPT-1775) | [1498-B] Render async update states | Backlog | OPT-1773 |
| [OPT-1776](https://linear.app/boostnlvp/issue/OPT-1776) | [1498-C] Verify download and staged-payload integrity | Backlog | OPT-1773, OPT-1769 |
| [OPT-1787](https://linear.app/boostnlvp/issue/OPT-1787) | [1352-F] Route committed-mutation reconciliation through the writer | Backlog | OPT-1634 |
| [OPT-1803](https://linear.app/boostnlvp/issue/OPT-1803) | [1243-A] Define semantic source-processing phases | Backlog | OPT-1708 |
| [OPT-1804](https://linear.app/boostnlvp/issue/OPT-1804) | [1243-B] Publish known counts and honest indeterminate progress | Backlog | OPT-1803 |
| [OPT-1805](https://linear.app/boostnlvp/issue/OPT-1805) | [1243-C] Project compact source-processing UI progress | Backlog | OPT-1804 |
| [OPT-1806](https://linear.app/boostnlvp/issue/OPT-1806) | [1243-D] Fence stale generations and coalesce cancellation | Backlog | OPT-1805, OPT-1803 |
| [OPT-1807](https://linear.app/boostnlvp/issue/OPT-1807) | [1243-E] Validate large-source progress and native evidence | Backlog | OPT-1806 |
| [OPT-1818](https://linear.app/boostnlvp/issue/OPT-1818) | [1356-B] Record cancellation at every pre-publish checkpoint | Backlog | OPT-1632 |
| [OPT-1819](https://linear.app/boostnlvp/issue/OPT-1819) | [1356-C] Release capacity only after capability-verified cleanup | Backlog | OPT-1818, OPT-1633 |
| [OPT-1820](https://linear.app/boostnlvp/issue/OPT-1820) | [1356-D] Retain staging and classify actionable cancellation retry | Backlog | OPT-1819 |
| [OPT-1821](https://linear.app/boostnlvp/issue/OPT-1821) | [1356-E] Validate typed cancellation under crash, low-space, and multi-volume faults | Backlog | OPT-1819, OPT-1820, OPT-1818 |
| [OPT-1828](https://linear.app/boostnlvp/issue/OPT-1828) | [1357-C] Route source-registry persistence through the global writer | Backlog | None; resolve stated scope |
| [OPT-1831](https://linear.app/boostnlvp/issue/OPT-1831) | [1357-E] Persist user-authored Harvest state through the writer | Backlog | OPT-1645 |
| [OPT-1833](https://linear.app/boostnlvp/issue/OPT-1833) | [1357-F] Persist operation links through the writer | Backlog | OPT-1644 |
| [OPT-1836](https://linear.app/boostnlvp/issue/OPT-1836) | [1357-H] Fence global commands by profile epoch and OperationId | Backlog | OPT-2031 |
| [OPT-1838](https://linear.app/boostnlvp/issue/OPT-1838) | [1357-I] Prove shutdown drain and global ownership diagnostics | Backlog | OPT-1836 |
| [OPT-1840](https://linear.app/boostnlvp/issue/OPT-1840) | [1358-A] Publish and verify the destination before reconciliation | Backlog | OPT-1643, OPT-1702, OPT-1632 |
| [OPT-1846](https://linear.app/boostnlvp/issue/OPT-1846) | [1358-B] Persist origin-removal intent and capability-bound absence | Backlog | OPT-1643, OPT-1633, OPT-1702 |
| [OPT-1848](https://linear.app/boostnlvp/issue/OPT-1848) | [1358-C] Retire origin membership only after absence proof | Backlog | OPT-1645, OPT-1643, OPT-1633, OPT-1702 |
| [OPT-1849](https://linear.app/boostnlvp/issue/OPT-1849) | [1358-D] Prove cross-source recovery and checkpoint replay dispositions | Backlog | OPT-1702, OPT-1645, OPT-1644, OPT-1643, OPT-1840, OPT-1632, OPT-1846, OPT-1633, OPT-1848 |
| [OPT-1851](https://linear.app/boostnlvp/issue/OPT-1851) | [1360-F] Qualify contention external-reader and WAL fixtures | Backlog | OPT-1681, OPT-1680 |
| [OPT-1852](https://linear.app/boostnlvp/issue/OPT-1852) | [1360-G] Build bounded durability latency harness | Backlog | OPT-1680, OPT-1683 |
| [OPT-1853](https://linear.app/boostnlvp/issue/OPT-1853) | [1360-H] Produce reproducible SLO workload and threshold evidence | Backlog | OPT-1683, OPT-1852 |
| [OPT-1854](https://linear.app/boostnlvp/issue/OPT-1854) | [1360-I] Gate stronger durability claims by qualified combination | Backlog | OPT-1853, OPT-1845, OPT-1851, OPT-1847, OPT-1684, OPT-1850, OPT-1844 |
| [OPT-1898](https://linear.app/boostnlvp/issue/OPT-1898) | [1499-H] Restore Harvest status filter in Yield GPUI | Backlog | OPT-1900 |
| [OPT-1900](https://linear.app/boostnlvp/issue/OPT-1900) | [1499-H1] Expose bounded Harvest facts for the GPUI status filter | Backlog | OPT-1645, OPT-1831 |
| [OPT-1907](https://linear.app/boostnlvp/issue/OPT-1907) | [1499-J] Restore independent browser filter-family enable controls in Yield | In Progress | OPT-1898 |
| [OPT-1922](https://linear.app/boostnlvp/issue/OPT-1922) | GPUI: bind one analysis feature job to source-owned artifact completion | Backlog | OPT-1655 |
| [OPT-1990](https://linear.app/boostnlvp/issue/OPT-1990) | Linux: requalify held Ubuntu release validation commands for current GPUI | Backlog | OPT-1863 |
| [OPT-2008](https://linear.app/boostnlvp/issue/OPT-2008) | GPUI: expose source-owned unsupported-file list and safe reveal modal | Backlog | OPT-2036 |
| [OPT-2012](https://linear.app/boostnlvp/issue/OPT-2012) | Tag mutations: add journal-backed definition and assignment participant boundary | Backlog | OPT-2034 |

