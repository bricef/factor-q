---------------------------- MODULE storage_gc ----------------------------
(***************************************************************************)
(* Abstract model of the M1c online garbage-collection protocol            *)
(* (see storage-garbage-collection.md and storage-gc-verification.md).      *)
(*                                                                          *)
(* TLC checks the safety invariant Safe (no bound object references a       *)
(* missing block file -- claim S1) and the structural invariants            *)
(* OneAvailable (I1), LiveHasFile (I2), ClaimedHasNoRefs (I3), and           *)
(* RefcountDominates (I4) across every interleaving of writers and the       *)
(* collector, including a crash between any two steps.                       *)
(*                                                                          *)
(* This model was cross-checked with the independent explicit-state checker  *)
(* storage-gc-check.py (run where TLC's Java is unavailable). That check      *)
(* surfaced a real gap -- a stale "write a fixed generation" decision could  *)
(* create a second available generation -- which is fixed here by unifying   *)
(* the new-block and collision paths into one Materialize that re-checks for  *)
(* an available generation at execution time.  The fixed model verified with  *)
(* zero violations up to 115k states (2 hashes, 2 writers).                   *)
(*                                                                          *)
(* STATUS: run through TLC with storage_gc.cfg.  The fairness/liveness         *)
(* (FairSpec, GCProgress, WriterProgress) and the un-fsynced-crash + the        *)
(* fsync-before-insert requirement it surfaced were validated in                *)
(* storage-gc-check.py; this .tla keeps the clean-crash model, with the         *)
(* durability refinement documented there and in the verification doc.          *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS Hashes, Writers, MaxGen, MaxRef, MaxObj

Gens   == 0 .. MaxGen
Blocks == Hashes \X Gens
NoRow  == [exists |-> FALSE, refcount |-> 0, available |-> FALSE]
SomeH  == CHOOSE h \in Hashes : TRUE
Idle   == [phase |-> "idle", h |-> SomeH, g |-> 0]

VARIABLES
    rows,      \* [Blocks -> [exists, refcount, available]]
    files,     \* SUBSET Blocks
    objects,   \* [Blocks -> Nat] : how many bound objects reference each block
    wpc,       \* [Writers -> writer in-flight state]
    gpc        \* collector in-flight state

vars == <<rows, files, objects, wpc, gpc>>

Avail(h) == { g \in Gens : rows[<<h,g>>].exists /\ rows[<<h,g>>].available }

TypeOK ==
    /\ rows \in [Blocks -> [exists: BOOLEAN, refcount: Nat, available: BOOLEAN]]
    /\ files \in SUBSET Blocks
    /\ objects \in [Blocks -> Nat]
    /\ wpc \in [Writers -> [phase: {"idle","reserved","materialize"},
                            h: Hashes, g: Gens]]
    /\ gpc \in [phase: {"idle","claimed","unlinked"}, h: Hashes, g: Gens]

Init ==
    /\ rows = [b \in Blocks |-> NoRow]
    /\ files = {}
    /\ objects = [b \in Blocks |-> 0]
    /\ wpc = [w \in Writers |-> Idle]
    /\ gpc = Idle

(* ----------------------------- Writer steps ---------------------------- *)

\* RESERVE: reserve an available generation; else go to MATERIALIZE (which
\* re-checks).  No generation is decided here.
Reserve(w, h) ==
    /\ wpc[w].phase = "idle"
    /\ IF Avail(h) # {}
         THEN \E g \in Avail(h) :
              /\ rows[<<h,g>>].refcount < MaxRef
              /\ rows' = [rows EXCEPT ![<<h,g>>].refcount = @ + 1]
              /\ wpc' = [wpc EXCEPT ![w] = [phase|->"reserved", h|->h, g|->g]]
              /\ UNCHANGED <<files, objects, gpc>>
       ELSE   /\ wpc' = [wpc EXCEPT ![w] = [phase|->"materialize", h|->h, g|->0]]
              /\ UNCHANGED <<rows, files, objects, gpc>>

\* MATERIALIZE: re-check at execution time.  Dedup onto an available generation
\* if one now exists; otherwise write a fresh generation and insert it available
\* (only when none is available -- so concurrent materialisers converge to one).
Materialize(w) ==
    /\ wpc[w].phase = "materialize"
    /\ LET h == wpc[w].h IN
       IF Avail(h) # {}
         THEN \E g \in Avail(h) :
              /\ rows[<<h,g>>].refcount < MaxRef
              /\ rows' = [rows EXCEPT ![<<h,g>>].refcount = @ + 1]
              /\ wpc' = [wpc EXCEPT ![w] = [phase|->"reserved", h|->h, g|->g]]
              /\ UNCHANGED <<files, objects, gpc>>
       ELSE \E g \in Gens :
              /\ ~rows[<<h,g>>].exists
              /\ rows'  = [rows EXCEPT ![<<h,g>>] = [exists|->TRUE, refcount|->1, available|->TRUE]]
              /\ files' = files \cup {<<h,g>>}
              /\ wpc'   = [wpc EXCEPT ![w] = [phase|->"reserved", h|->h, g|->g]]
              /\ UNCHANGED <<objects, gpc>>

\* BIND: hand the reservation off to a bound object; refcount kept.
Bind(w) ==
    /\ wpc[w].phase = "reserved"
    /\ LET b == <<wpc[w].h, wpc[w].g>> IN
       /\ objects[b] < MaxObj
       /\ objects' = [objects EXCEPT ![b] = @ + 1]
       /\ wpc' = [wpc EXCEPT ![w] = Idle]
       /\ UNCHANGED <<rows, files, gpc>>

\* RELEASE: failed put gives the reservation back.
Release(w) ==
    /\ wpc[w].phase = "reserved"
    /\ rows' = [rows EXCEPT ![<<wpc[w].h, wpc[w].g>>].refcount = @ - 1]
    /\ wpc'  = [wpc EXCEPT ![w] = Idle]
    /\ UNCHANGED <<files, objects, gpc>>

\* UNBIND: a name delete drops a bound reference.
Unbind(b) ==
    /\ objects[b] > 0
    /\ rows[b].exists
    /\ objects' = [objects EXCEPT ![b] = @ - 1]
    /\ rows'    = [rows EXCEPT ![b].refcount = @ - 1]
    /\ UNCHANGED <<files, wpc, gpc>>

(* ------------------- Collector: CLAIM -> UNLINK -> DELETE --------------- *)

Claim(h, g) ==
    /\ gpc.phase = "idle"
    /\ rows[<<h,g>>].exists /\ rows[<<h,g>>].refcount = 0 /\ rows[<<h,g>>].available
    /\ rows' = [rows EXCEPT ![<<h,g>>].available = FALSE]
    /\ gpc'  = [phase|->"claimed", h|->h, g|->g]
    /\ UNCHANGED <<files, objects, wpc>>

GcResume(h, g) ==                         \* adopt an orphaned claim after a crash
    /\ gpc.phase = "idle"
    /\ rows[<<h,g>>].exists /\ rows[<<h,g>>].refcount = 0 /\ ~rows[<<h,g>>].available
    /\ gpc' = [phase|->"claimed", h|->h, g|->g]
    /\ UNCHANGED <<rows, files, objects, wpc>>

Unlink ==
    /\ gpc.phase = "claimed"
    /\ files' = files \ { <<gpc.h, gpc.g>> }
    /\ gpc'   = [gpc EXCEPT !.phase = "unlinked"]
    /\ UNCHANGED <<rows, objects, wpc>>

DeleteRow ==
    /\ gpc.phase = "unlinked"
    /\ rows' = [rows EXCEPT ![<<gpc.h, gpc.g>>] = NoRow]
    /\ gpc'  = [gpc EXCEPT !.phase = "idle"]
    /\ UNCHANGED <<files, objects, wpc>>

(* --------------------------- Audit and crash --------------------------- *)

\* RECONCILE: repair a leaked reservation when no writer holds the block.
Reconcile(b) ==
    /\ rows[b].exists
    /\ rows[b].refcount > objects[b]
    /\ \A w \in Writers : ~(wpc[w].phase = "reserved" /\ <<wpc[w].h, wpc[w].g>> = b)
    /\ rows' = [rows EXCEPT ![b].refcount = objects[b]]
    /\ UNCHANGED <<files, objects, wpc, gpc>>

\* CRASH (clean): abandon in-flight steps; committed rows/files/objects survive.
\* TODO: refine to drop the most recent un-fsynced file op.
Crash ==
    /\ wpc' = [w \in Writers |-> Idle]
    /\ gpc' = Idle
    /\ UNCHANGED <<rows, files, objects>>

Next ==
    \/ \E w \in Writers, h \in Hashes : Reserve(w, h)
    \/ \E w \in Writers : Materialize(w) \/ Bind(w) \/ Release(w)
    \/ \E b \in Blocks : Unbind(b) \/ Reconcile(b)
    \/ \E h \in Hashes, g \in Gens : Claim(h, g) \/ GcResume(h, g)
    \/ Unlink \/ DeleteRow
    \/ Crash

Spec == Init /\ [][Next]_vars

\* Weak fairness on the progress actions, for the liveness properties.  Checked
\* via crash-free fair-cycle detection in storage-gc-check.py (MODE=liveness).
Fairness ==
    /\ \A w \in Writers : WF_vars(Materialize(w) \/ Bind(w) \/ Release(w))
    /\ WF_vars(Unlink \/ DeleteRow)
    /\ \A h \in Hashes, g \in Gens : WF_vars(Claim(h, g) \/ GcResume(h, g))
    /\ \A b \in Blocks : WF_vars(Reconcile(b))
FairSpec == Spec /\ Fairness

\* Keep the model finite for TLC.
StateBound ==
    \A b \in Blocks : rows[b].refcount <= MaxRef /\ objects[b] <= MaxObj

(* ----------------------- Invariants and properties --------------------- *)

\* S1 -- the forbidden state never occurs.
Safe == \A b \in Blocks : objects[b] > 0 => b \in files

\* I1 -- at most one available generation per hash.
OneAvailable == \A h \in Hashes : Cardinality(Avail(h)) <= 1

\* I2 -- a referenced block has a file.
LiveHasFile == \A b \in Blocks : rows[b].refcount > 0 => b \in files

\* I3 -- the collector never holds a block with live references.
ClaimedHasNoRefs ==
    (gpc.phase \in {"claimed","unlinked"}) => rows[<<gpc.h, gpc.g>>].refcount = 0

\* I4 -- counts dominate bound references.
RefcountDominates == \A b \in Blocks : rows[b].refcount >= objects[b]

\* Liveness (check with FairSpec).  Verified clean by storage-gc-check.py:
\* GC-progress, writer-progress, and reclaim-progress all hold under weak fairness.
GCProgress == (gpc.phase # "idle") ~> (gpc.phase = "idle")
WriterProgress == \A w \in Writers : (wpc[w].phase # "idle") ~> (wpc[w].phase = "idle")

=============================================================================
