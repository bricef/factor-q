---------------------------- MODULE storage_gc ----------------------------
(***************************************************************************)
(* Abstract model of the M1c online garbage-collection protocol            *)
(* (see storage-garbage-collection.md and storage-gc-verification.md).      *)
(*                                                                          *)
(* TLC checks the safety invariant Safe (no bound object references a       *)
(* missing block file -- claim S1) and the structural invariants            *)
(* OneAvailable (I1) and ClaimedHasNoRefs (I3) across every interleaving of  *)
(* writers and the collector, including a crash between any two steps.       *)
(*                                                                          *)
(* STATUS: skeleton to complete and run, not a finished proof.  TODOs:      *)
(*   - weak fairness (WF_vars) on GC/writer actions for the liveness props; *)
(*   - refine Crash to model un-fsynced loss (drop the most recent unsynced *)
(*     file op) -- the clean-crash model below understates I2/I3 stress;     *)
(*   - give bound objects identities so two objects can name the same block  *)
(*     (the singleton-set model is adequate for Safe, not for the I4/        *)
(*     Reconcile accounting).                                                *)
(*                                                                          *)
(* Suggested TLC model: Hashes = {"h1","h2"}, Writers = {"w1","w2"},        *)
(* MaxGen = 2.  Invariants: TypeOK, Safe, OneAvailable, ClaimedHasNoRefs.    *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Hashes, Writers, MaxGen

Gens   == 0 .. MaxGen
Blocks == Hashes \X Gens
NoRow  == [exists |-> FALSE, refcount |-> 0, available |-> FALSE]
SomeH  == CHOOSE h \in Hashes : TRUE          \* placeholder for idle h/g fields

VARIABLES
    rows,      \* [Blocks -> [exists, refcount, available]]
    files,     \* SUBSET Blocks : which block files exist on disk
    objects,   \* SUBSET (SUBSET Blocks) : bound objects, each a set of blocks
    wpc,       \* [Writers -> writer in-flight state]
    gpc        \* collector in-flight state

vars == <<rows, files, objects, wpc, gpc>>

Avail(h) == { g \in Gens : rows[<<h,g>>].exists /\ rows[<<h,g>>].available }
Refs(b)  == Cardinality({ o \in objects : b \in o })   \* bound objects naming b

TypeOK ==
    /\ rows \in [Blocks -> [exists: BOOLEAN, refcount: Nat, available: BOOLEAN]]
    /\ files \in SUBSET Blocks
    /\ objects \in SUBSET (SUBSET Blocks)
    /\ wpc \in [Writers -> [phase: {"idle","reserved","needNew","collision"},
                            h: Hashes, g: Gens]]
    /\ gpc \in [phase: {"idle","claimed","unlinked"}, h: Hashes, g: Gens]

Idle == [phase |-> "idle", h |-> SomeH, g |-> 0]

Init ==
    /\ rows = [b \in Blocks |-> NoRow]
    /\ files = {}
    /\ objects = {}
    /\ wpc = [w \in Writers |-> Idle]
    /\ gpc = Idle

(* ----------------------------- Writer steps ---------------------------- *)

\* RESERVE: w starts a put for some hash h; dispatch on the index state.
Reserve(w, h) ==
    /\ wpc[w].phase = "idle"
    /\ IF Avail(h) # {}
         THEN \E g \in Avail(h) :                       \* reserve current gen
              /\ rows' = [rows EXCEPT ![<<h,g>>].refcount = @ + 1]
              /\ wpc' = [wpc EXCEPT ![w] = [phase|->"reserved", h|->h, g|->g]]
              /\ UNCHANGED <<files, objects, gpc>>
       ELSE IF \A g \in Gens : ~rows[<<h,g>>].exists
         THEN /\ wpc' = [wpc EXCEPT ![w] = [phase|->"needNew", h|->h, g|->0]]
              /\ UNCHANGED <<rows, files, objects, gpc>>
       ELSE                                             \* claimed -> collision
              /\ wpc' = [wpc EXCEPT ![w] = [phase|->"collision", h|->h, g|->0]]
              /\ UNCHANGED <<rows, files, objects, gpc>>

\* WRITE_FILE: materialise a brand-new block (gen 0) and insert its row.
WriteNew(w) ==
    /\ wpc[w].phase = "needNew"
    /\ LET h == wpc[w].h IN
       /\ ~rows[<<h,0>>].exists
       /\ rows'  = [rows EXCEPT ![<<h,0>>] = [exists|->TRUE, refcount|->1, available|->TRUE]]
       /\ files' = files \cup {<<h,0>>}
       /\ wpc'   = [wpc EXCEPT ![w] = [phase|->"reserved", h|->h, g|->0]]
       /\ UNCHANGED <<objects, gpc>>

\* MINT: collision -> write a fresh generation; insert an available row only if
\* none exists, so concurrent minters converge (the later one dedups onto it).
Mint(w) ==
    /\ wpc[w].phase = "collision"
    /\ LET h == wpc[w].h IN
       IF Avail(h) # {}
         THEN \E g \in Avail(h) :
              /\ rows' = [rows EXCEPT ![<<h,g>>].refcount = @ + 1]
              /\ wpc' = [wpc EXCEPT ![w] = [phase|->"reserved", h|->h, g|->g]]
              /\ UNCHANGED <<files, objects, gpc>>
       ELSE \E g \in Gens :
              /\ ~rows[<<h,g>>].exists
              /\ rows'  = [rows EXCEPT ![<<h,g>>] = [exists|->TRUE, refcount|->1, available|->TRUE]]
              /\ files' = files \cup {<<h,g>>}
              /\ wpc'   = [wpc EXCEPT ![w] = [phase|->"reserved", h|->h, g|->g]]
              /\ UNCHANGED <<objects, gpc>>

\* BIND: hand the reservation off to a (single-block) bound object; refcount kept.
Bind(w) ==
    /\ wpc[w].phase = "reserved"
    /\ objects' = objects \cup { {<<wpc[w].h, wpc[w].g>>} }
    /\ wpc' = [wpc EXCEPT ![w] = Idle]
    /\ UNCHANGED <<rows, files, gpc>>

\* RELEASE: the put fails before binding; give the reservation back.
Release(w) ==
    /\ wpc[w].phase = "reserved"
    /\ rows' = [rows EXCEPT ![<<wpc[w].h, wpc[w].g>>].refcount = @ - 1]
    /\ wpc'  = [wpc EXCEPT ![w] = Idle]
    /\ UNCHANGED <<files, objects, gpc>>

(* ------------------- Collector steps: CLAIM->UNLINK->DELETE ------------- *)

Claim(h, g) ==
    /\ gpc.phase = "idle"
    /\ rows[<<h,g>>].exists /\ rows[<<h,g>>].refcount = 0 /\ rows[<<h,g>>].available
    /\ rows' = [rows EXCEPT ![<<h,g>>].available = FALSE]
    /\ gpc'  = [phase|->"claimed", h|->h, g|->g]
    /\ UNCHANGED <<files, objects, wpc>>

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

\* RESUME: after a crash, GC adopts an orphaned claim (available=false, ref 0).
GcResume(h, g) ==
    /\ gpc.phase = "idle"
    /\ rows[<<h,g>>].exists /\ rows[<<h,g>>].refcount = 0 /\ ~rows[<<h,g>>].available
    /\ gpc' = [phase|->"claimed", h|->h, g|->g]
    /\ UNCHANGED <<rows, files, objects, wpc>>

(* --------------------------- Audit and crash --------------------------- *)

\* RECONCILE: the audit repairs a leaked reservation (refcount above the object
\* count) when no writer currently holds the block reserved.
Reconcile(b) ==
    /\ rows[b].exists
    /\ rows[b].refcount > Refs(b)
    /\ \A w \in Writers : ~(wpc[w].phase = "reserved" /\ <<wpc[w].h, wpc[w].g>> = b)
    /\ rows' = [rows EXCEPT ![b].refcount = Refs(b)]
    /\ UNCHANGED <<files, objects, wpc, gpc>>

\* CRASH (clean): abandon all in-flight steps; committed rows/files/objects
\* survive.  TODO: refine to also drop the most recent un-fsynced file op.
Crash ==
    /\ wpc' = [w \in Writers |-> Idle]
    /\ gpc' = Idle
    /\ UNCHANGED <<rows, files, objects>>

Next ==
    \/ \E w \in Writers, h \in Hashes : Reserve(w, h)
    \/ \E w \in Writers : WriteNew(w) \/ Mint(w) \/ Bind(w) \/ Release(w)
    \/ \E h \in Hashes, g \in Gens : Claim(h, g) \/ GcResume(h, g)
    \/ Unlink \/ DeleteRow
    \/ \E b \in Blocks : Reconcile(b)
    \/ Crash

\* TODO: add WF_vars on the GC/writer-progress actions for the liveness props.
Spec == Init /\ [][Next]_vars

(* ----------------------- Invariants and properties --------------------- *)

\* S1 -- the forbidden state never occurs.
Safe == \A o \in objects : \A b \in o : b \in files

\* I1 -- at most one available generation per hash.
OneAvailable == \A h \in Hashes : Cardinality(Avail(h)) <= 1

\* I3 -- the collector never holds a block with live references.
ClaimedHasNoRefs ==
    (gpc.phase \in {"claimed","unlinked"}) => rows[<<gpc.h, gpc.g>>].refcount = 0

\* L1 (needs fairness) -- a dead block is eventually reclaimed.
\* EventuallyReclaimed ==
\*     \A b \in Blocks : (rows[b].exists /\ rows[b].refcount = 0) ~> (~rows[b].exists)

=============================================================================
