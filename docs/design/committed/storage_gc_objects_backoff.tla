------------------------ MODULE storage_gc_objects_backoff ------------------------
(***************************************************************************)
(* Object/manifest reclaim — the BACK-OFF variant (issue #173).            *)
(*                                                                          *)
(* The block model (storage_gc.tla) abstracts objects to a per-block count  *)
(* and cannot express the object-level forbidden state: a live NAME whose   *)
(* MANIFEST file is gone. This model adds the missing layer — object rows,  *)
(* manifest files, and name references — and checks the object safety       *)
(* invariant SafeObj across every interleaving of writers and the collector *)
(* (crash between any two steps included).                                  *)
(*                                                                          *)
(* Structurally this IS the block protocol (reserve -> materialize -> bind, *)
(* with the collector's claim -> unlink -> delete) with two changes:        *)
(*   1. objects carry NO generation — a manifest is content-addressed, one  *)
(*      per cid forever;                                                     *)
(*   2. a writer that meets a CLAIMED object BACKS OFF and retries once the  *)
(*      collector has finished, rather than minting a fresh generation.      *)
(* Compare storage_gc_objects_gen.tla, which keeps the generation axis and   *)
(* never blocks a writer. The safety question this file answers: does        *)
(* reserve-before-rely, WITHOUT generations, keep SafeObj across every        *)
(* interleaving? (An earlier no-reserve sketch did not — TLC found a stale    *)
(* manifest-write racing a full collect cycle; the reservation closes it.)    *)
(*                                                                          *)
(* The block LAYER is deliberately absent: whether a bind's block            *)
(* reservation succeeds or fails cannot change manifest presence, so the     *)
(* object invariant is provable without it (assume-guarantee — the block     *)
(* model already proves that layer). See storage-gc-objects-verification.md. *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS Cids, Writers, MaxRef, MaxName, MaxCrash

NoRow == [exists |-> FALSE, refcount |-> 0, available |-> FALSE]
SomeC == CHOOSE c \in Cids : TRUE
Idle  == [phase |-> "idle", c |-> SomeC]

VARIABLES
    orows,      \* [Cids -> [exists, refcount, available]] : the objects-table row.
                \*   refcount = in-flight reservations + bound names.
    manifests,  \* SUBSET Cids : manifest files on disk (content-addressed, one per cid)
    nameRefs,   \* [Cids -> Nat] : bound name-versions referencing this cid
    wpc,        \* [Writers -> put in-flight state]
    gpc,        \* collector in-flight state
    crashes     \* count of crashes so far (bounded by MaxCrash, for liveness)

vars  == <<orows, manifests, nameRefs, wpc, gpc, crashes>>
pvars == <<orows, manifests, nameRefs, wpc, gpc>>

TypeOK ==
    /\ orows \in [Cids -> [exists: BOOLEAN, refcount: Nat, available: BOOLEAN]]
    /\ manifests \in SUBSET Cids
    /\ nameRefs \in [Cids -> Nat]
    /\ wpc \in [Writers -> [phase: {"idle","reserved","materialize","backoff"}, c: Cids]]
    /\ gpc \in [phase: {"idle","claimed","unlinked"}, c: Cids]
    /\ crashes \in 0 .. MaxCrash

Init ==
    /\ orows = [c \in Cids |-> NoRow]
    /\ manifests = {}
    /\ nameRefs = [c \in Cids |-> 0]
    /\ wpc = [w \in Writers |-> Idle]
    /\ gpc = Idle
    /\ crashes = 0

(* ----------------------------- Writer: put ----------------------------- *)

\* RESERVE: grab the object on its available state (refcount + 1, reserve-before-
\* rely). A claimed object -> back off; an absent object -> materialize (create it).
Reserve(w, c) ==
    /\ wpc[w].phase = "idle"
    /\ IF orows[c].exists /\ orows[c].available
         THEN /\ orows[c].refcount < MaxRef
              /\ orows' = [orows EXCEPT ![c].refcount = @ + 1]
              /\ wpc'  = [wpc EXCEPT ![w] = [phase |-> "reserved", c |-> c]]
              /\ UNCHANGED <<manifests, nameRefs, gpc>>
       ELSE IF orows[c].exists /\ ~orows[c].available
         THEN /\ wpc' = [wpc EXCEPT ![w] = [phase |-> "backoff", c |-> c]]
              /\ UNCHANGED <<orows, manifests, nameRefs, gpc>>
       ELSE   /\ wpc' = [wpc EXCEPT ![w] = [phase |-> "materialize", c |-> c]]
              /\ UNCHANGED <<orows, manifests, nameRefs, gpc>>

\* MATERIALIZE: re-check at execution time. Dedup onto an existing available object;
\* back off if it was claimed meanwhile; otherwise create the row at refcount 1 AND
\* write its manifest in one step. The row is born reserved (refcount 1), so the
\* manifest is protected from the instant it exists — the collector cannot claim a
\* refcount>0 object, hence cannot unlink the manifest. (A crash between the real
\* write_object and the row insert leaves a manifest with no row: leak-safe, never
\* S1, reclaimed by the audit's orphan reaper — modelled abstractly, as blocks are.)
Materialize(w) ==
    /\ wpc[w].phase = "materialize"
    /\ LET c == wpc[w].c IN
       IF orows[c].exists /\ orows[c].available
         THEN /\ orows[c].refcount < MaxRef
              /\ orows' = [orows EXCEPT ![c].refcount = @ + 1]
              /\ wpc'  = [wpc EXCEPT ![w] = [phase |-> "reserved", c |-> c]]
              /\ UNCHANGED <<manifests, nameRefs, gpc>>
       ELSE IF orows[c].exists /\ ~orows[c].available
         THEN /\ wpc' = [wpc EXCEPT ![w] = [phase |-> "backoff", c |-> c]]
              /\ UNCHANGED <<orows, manifests, nameRefs, gpc>>
       ELSE   /\ orows'     = [orows EXCEPT ![c] = [exists |-> TRUE, refcount |-> 1, available |-> TRUE]]
              /\ manifests' = manifests \cup {c}
              /\ wpc'       = [wpc EXCEPT ![w] = [phase |-> "reserved", c |-> c]]
              /\ UNCHANGED <<nameRefs, gpc>>

\* BACKOFF: the object is mid-collection; yield this attempt to idle. The caller
\* retries (a fresh Reserve) — eventually the collector's DeleteRow makes the cid
\* absent and the retry materializes it afresh.
Backoff(w) ==
    /\ wpc[w].phase = "backoff"
    /\ wpc' = [wpc EXCEPT ![w] = Idle]
    /\ UNCHANGED <<orows, manifests, nameRefs, gpc>>

\* BIND: hand the reservation off to a bound name (refcount already counts it).
Bind(w) ==
    /\ wpc[w].phase = "reserved"
    /\ LET c == wpc[w].c IN
       /\ nameRefs[c] < MaxName
       /\ nameRefs' = [nameRefs EXCEPT ![c] = @ + 1]
       /\ wpc' = [wpc EXCEPT ![w] = Idle]
       /\ UNCHANGED <<orows, manifests, gpc>>

\* RELEASE: a failed put gives the reservation back.
Release(w) ==
    /\ wpc[w].phase = "reserved"
    /\ orows' = [orows EXCEPT ![wpc[w].c].refcount = @ - 1]
    /\ wpc'  = [wpc EXCEPT ![w] = Idle]
    /\ UNCHANGED <<manifests, nameRefs, gpc>>

\* UNBIND: a name delete drops one bound reference (and its refcount).
Unbind(c) ==
    /\ nameRefs[c] > 0
    /\ orows[c].exists
    /\ nameRefs' = [nameRefs EXCEPT ![c] = @ - 1]
    /\ orows'    = [orows EXCEPT ![c].refcount = @ - 1]
    /\ UNCHANGED <<manifests, wpc, gpc>>

(* ------------------ Collector: CLAIM -> UNLINK -> DELETE ---------------- *)

\* CLAIM the object (the GC compare-and-swap): available -> false, conditional on
\* refcount = 0 and available. A writer that reserved first (refcount > 0) makes it fail.
ClaimObj(c) ==
    /\ gpc.phase = "idle"
    /\ orows[c].exists /\ orows[c].refcount = 0 /\ orows[c].available
    /\ orows' = [orows EXCEPT ![c].available = FALSE]
    /\ gpc'  = [phase |-> "claimed", c |-> c]
    /\ UNCHANGED <<manifests, nameRefs, wpc>>

\* Adopt an orphaned claim (a crash mid-reclaim: claimed, refcount 0, collector idle).
GcResumeObj(c) ==
    /\ gpc.phase = "idle"
    /\ orows[c].exists /\ orows[c].refcount = 0 /\ ~orows[c].available
    /\ gpc' = [phase |-> "claimed", c |-> c]
    /\ UNCHANGED <<orows, manifests, nameRefs, wpc>>

\* UNLINK the manifest — only on a claimed object, so no writer can reserve it and
\* no name can be resurrected over it while the file goes away.
UnlinkManifest ==
    /\ gpc.phase = "claimed"
    /\ manifests' = manifests \ { gpc.c }
    /\ gpc'       = [gpc EXCEPT !.phase = "unlinked"]
    /\ UNCHANGED <<orows, nameRefs, wpc>>

\* DELETE the row after the manifest is gone; the object returns to absent.
DeleteObjRow ==
    /\ gpc.phase = "unlinked"
    /\ orows' = [orows EXCEPT ![gpc.c] = NoRow]
    /\ gpc'  = [gpc EXCEPT !.phase = "idle"]
    /\ UNCHANGED <<manifests, nameRefs, wpc>>

(* --------------------------- Audit and crash --------------------------- *)

\* RECONCILE: repair a leaked reservation (refcount above the bound-name count)
\* when no writer holds the object — the audit's quiescent repair, as for blocks.
Reconcile(c) ==
    /\ orows[c].exists
    /\ orows[c].refcount > nameRefs[c]
    /\ \A w \in Writers : ~(wpc[w].phase = "reserved" /\ wpc[w].c = c)
    /\ orows' = [orows EXCEPT ![c].refcount = nameRefs[c]]
    /\ UNCHANGED <<manifests, nameRefs, wpc, gpc>>

\* CRASH (clean): abandon in-flight steps; committed rows / manifests / names survive.
\* (The un-fsynced refinement — a manifest durable before the row that names it —
\* is stated in the verification doc, as for blocks.)
Crash ==
    /\ crashes < MaxCrash
    /\ crashes' = crashes + 1
    /\ wpc' = [w \in Writers |-> Idle]
    /\ gpc' = Idle
    /\ UNCHANGED <<orows, manifests, nameRefs>>

NonCrashNext ==
    \/ \E w \in Writers, c \in Cids : Reserve(w, c)
    \/ \E w \in Writers : Materialize(w) \/ Backoff(w) \/ Bind(w) \/ Release(w)
    \/ \E c \in Cids : Unbind(c) \/ Reconcile(c)
    \/ \E c \in Cids : ClaimObj(c) \/ GcResumeObj(c)
    \/ UnlinkManifest \/ DeleteObjRow

Next == (NonCrashNext /\ UNCHANGED crashes) \/ Crash

Spec == Init /\ [][Next]_vars

\* Fairness for liveness. The collector's claim/resume is STRONG-fair (it models the
\* reachability audit, which visits every object); writers, the in-flight
\* unlink/delete, and reconcile are weak-fair. Same rationale as the block model.
Fairness ==
    /\ \A w \in Writers : WF_pvars(Materialize(w) \/ Backoff(w) \/ Bind(w) \/ Release(w))
    /\ WF_pvars(UnlinkManifest \/ DeleteObjRow)
    /\ \A c \in Cids : SF_pvars(ClaimObj(c) \/ GcResumeObj(c))
    /\ \A c \in Cids : SF_pvars(Reconcile(c))
FairSpec == Spec /\ Fairness

(* ----------------------- Invariants and properties --------------------- *)

\* SafeObj — the object forbidden state (S1 for objects) never occurs: every live
\* name resolves to a manifest that is present.
SafeObj == \A c \in Cids : nameRefs[c] > 0 => c \in manifests

\* Inductive core: a live block has a file (here: a referenced object has a manifest).
LiveHasManifest == \A c \in Cids : orows[c].refcount > 0 => c \in manifests

\* The collector never holds an object with live references.
ClaimedHasNoRefs ==
    (gpc.phase \in {"claimed","unlinked"}) => orows[gpc.c].refcount = 0

\* Counts dominate bound names (any excess is an in-flight reservation).
RefcountDominates == \A c \in Cids : orows[c].refcount >= nameRefs[c]

\* Keep the model finite for TLC.
StateBound == \A c \in Cids : orows[c].refcount <= MaxRef /\ nameRefs[c] <= MaxName

(* Liveness: the collector finishes a claim it starts; every put step resolves;
   a dead object is eventually reclaimed or reused. *)
GCProgress == (gpc.phase # "idle") ~> (gpc.phase = "idle")
WriterProgress == \A w \in Writers : (wpc[w].phase # "idle") ~> (wpc[w].phase = "idle")
EventualReclaim ==
    \A c \in Cids : (orows[c].exists /\ orows[c].refcount = 0)
                        ~> (~orows[c].exists \/ orows[c].refcount > 0)

=============================================================================
