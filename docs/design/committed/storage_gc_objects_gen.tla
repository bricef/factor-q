-------------------------- MODULE storage_gc_objects_gen --------------------------
(***************************************************************************)
(* Object/manifest reclaim — the GENERATION variant (issue #173).          *)
(*                                                                          *)
(* The counterpart to storage_gc_objects_backoff.tla. Same object layer     *)
(* (object rows, manifest files, name references) and same safety question  *)
(* (SafeObj: a live name's manifest is always present), but objects carry a *)
(* GENERATION exactly as blocks do: a writer that meets a claimed object     *)
(* mints a FRESH generation (a new manifest at (cid, g+1)) instead of        *)
(* backing off, so a writer never waits on the collector.                    *)
(*                                                                          *)
(* This makes the object protocol structurally identical to the block        *)
(* protocol in storage_gc.tla — which is exactly the point of modelling it:  *)
(* the generation variant is "re-run the block protocol at the object        *)
(* layer," carrying the same OneAvailable (I1) machinery and unbounded        *)
(* generation tokens. Compare the back-off variant, which drops the          *)
(* generation axis at the cost of a bounded writer wait. See                 *)
(* storage-gc-objects-verification.md for the comparison and the choice.     *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS Cids, Writers, MaxGen, MaxRef, MaxName, MaxCrash

Gens   == 0 .. MaxGen
OKeys  == Cids \X Gens
NoRow  == [exists |-> FALSE, refcount |-> 0, available |-> FALSE]
SomeC  == CHOOSE c \in Cids : TRUE
Idle   == [phase |-> "idle", c |-> SomeC, g |-> 0]

VARIABLES
    rows,      \* [OKeys -> [exists, refcount, available]] : object-generation rows
    manifests, \* SUBSET OKeys : manifest files, keyed by (cid, generation)
    nameRefs,  \* [OKeys -> Nat] : names bound to each object-generation
    wpc,       \* [Writers -> put in-flight state]
    gpc,       \* collector in-flight state
    crashes

vars  == <<rows, manifests, nameRefs, wpc, gpc, crashes>>
pvars == <<rows, manifests, nameRefs, wpc, gpc>>

Avail(c) == { g \in Gens : rows[<<c,g>>].exists /\ rows[<<c,g>>].available }

TypeOK ==
    /\ rows \in [OKeys -> [exists: BOOLEAN, refcount: Nat, available: BOOLEAN]]
    /\ manifests \in SUBSET OKeys
    /\ nameRefs \in [OKeys -> Nat]
    /\ wpc \in [Writers -> [phase: {"idle","reserved","materialize"}, c: Cids, g: Gens]]
    /\ gpc \in [phase: {"idle","claimed","unlinked"}, c: Cids, g: Gens]
    /\ crashes \in 0 .. MaxCrash

Init ==
    /\ rows = [k \in OKeys |-> NoRow]
    /\ manifests = {}
    /\ nameRefs = [k \in OKeys |-> 0]
    /\ wpc = [w \in Writers |-> Idle]
    /\ gpc = Idle
    /\ crashes = 0

(* ----------------------------- Writer: put ----------------------------- *)

\* RESERVE an available generation of cid c; else go to MATERIALIZE (which mints).
Reserve(w, c) ==
    /\ wpc[w].phase = "idle"
    /\ IF Avail(c) # {}
         THEN \E g \in Avail(c) :
              /\ rows[<<c,g>>].refcount < MaxRef
              /\ rows' = [rows EXCEPT ![<<c,g>>].refcount = @ + 1]
              /\ wpc' = [wpc EXCEPT ![w] = [phase|->"reserved", c|->c, g|->g]]
              /\ UNCHANGED <<manifests, nameRefs, gpc>>
       ELSE   /\ wpc' = [wpc EXCEPT ![w] = [phase|->"materialize", c|->c, g|->0]]
              /\ UNCHANGED <<rows, manifests, nameRefs, gpc>>

\* MATERIALIZE: re-check. Dedup onto an available generation if one now exists;
\* otherwise MINT a fresh generation — write its manifest and insert the row
\* available, conditional on none being available (so concurrent minters converge).
\* A claimed generation's row still exists, so a fresh mint never reuses it: the
\* writer never blocks on the collector (the point of the generation axis).
Materialize(w) ==
    /\ wpc[w].phase = "materialize"
    /\ LET c == wpc[w].c IN
       IF Avail(c) # {}
         THEN \E g \in Avail(c) :
              /\ rows[<<c,g>>].refcount < MaxRef
              /\ rows' = [rows EXCEPT ![<<c,g>>].refcount = @ + 1]
              /\ wpc' = [wpc EXCEPT ![w] = [phase|->"reserved", c|->c, g|->g]]
              /\ UNCHANGED <<manifests, nameRefs, gpc>>
       ELSE \E g \in Gens :
              /\ ~rows[<<c,g>>].exists
              /\ rows'      = [rows EXCEPT ![<<c,g>>] = [exists|->TRUE, refcount|->1, available|->TRUE]]
              /\ manifests' = manifests \cup {<<c,g>>}
              /\ wpc'       = [wpc EXCEPT ![w] = [phase|->"reserved", c|->c, g|->g]]
              /\ UNCHANGED <<nameRefs, gpc>>

\* BIND: hand the reservation off to a bound name.
Bind(w) ==
    /\ wpc[w].phase = "reserved"
    /\ LET k == <<wpc[w].c, wpc[w].g>> IN
       /\ nameRefs[k] < MaxName
       /\ nameRefs' = [nameRefs EXCEPT ![k] = @ + 1]
       /\ wpc' = [wpc EXCEPT ![w] = Idle]
       /\ UNCHANGED <<rows, manifests, gpc>>

\* RELEASE: a failed put gives the reservation back.
Release(w) ==
    /\ wpc[w].phase = "reserved"
    /\ rows' = [rows EXCEPT ![<<wpc[w].c, wpc[w].g>>].refcount = @ - 1]
    /\ wpc'  = [wpc EXCEPT ![w] = Idle]
    /\ UNCHANGED <<manifests, nameRefs, gpc>>

\* UNBIND: a name delete drops one bound reference (and its refcount).
Unbind(k) ==
    /\ nameRefs[k] > 0
    /\ rows[k].exists
    /\ nameRefs' = [nameRefs EXCEPT ![k] = @ - 1]
    /\ rows'     = [rows EXCEPT ![k].refcount = @ - 1]
    /\ UNCHANGED <<manifests, wpc, gpc>>

(* ------------------ Collector: CLAIM -> UNLINK -> DELETE ---------------- *)

ClaimObj(c, g) ==
    /\ gpc.phase = "idle"
    /\ rows[<<c,g>>].exists /\ rows[<<c,g>>].refcount = 0 /\ rows[<<c,g>>].available
    /\ rows' = [rows EXCEPT ![<<c,g>>].available = FALSE]
    /\ gpc'  = [phase|->"claimed", c|->c, g|->g]
    /\ UNCHANGED <<manifests, nameRefs, wpc>>

GcResumeObj(c, g) ==
    /\ gpc.phase = "idle"
    /\ rows[<<c,g>>].exists /\ rows[<<c,g>>].refcount = 0 /\ ~rows[<<c,g>>].available
    /\ gpc' = [phase|->"claimed", c|->c, g|->g]
    /\ UNCHANGED <<rows, manifests, nameRefs, wpc>>

UnlinkManifest ==
    /\ gpc.phase = "claimed"
    /\ manifests' = manifests \ { <<gpc.c, gpc.g>> }
    /\ gpc'       = [gpc EXCEPT !.phase = "unlinked"]
    /\ UNCHANGED <<rows, nameRefs, wpc>>

DeleteObjRow ==
    /\ gpc.phase = "unlinked"
    /\ rows' = [rows EXCEPT ![<<gpc.c, gpc.g>>] = NoRow]
    /\ gpc'  = [gpc EXCEPT !.phase = "idle"]
    /\ UNCHANGED <<manifests, nameRefs, wpc>>

(* --------------------------- Audit and crash --------------------------- *)

Reconcile(k) ==
    /\ rows[k].exists
    /\ rows[k].refcount > nameRefs[k]
    /\ \A w \in Writers : ~(wpc[w].phase = "reserved" /\ <<wpc[w].c, wpc[w].g>> = k)
    /\ rows' = [rows EXCEPT ![k].refcount = nameRefs[k]]
    /\ UNCHANGED <<manifests, nameRefs, wpc, gpc>>

Crash ==
    /\ crashes < MaxCrash
    /\ crashes' = crashes + 1
    /\ wpc' = [w \in Writers |-> Idle]
    /\ gpc' = Idle
    /\ UNCHANGED <<rows, manifests, nameRefs>>

NonCrashNext ==
    \/ \E w \in Writers, c \in Cids : Reserve(w, c)
    \/ \E w \in Writers : Materialize(w) \/ Bind(w) \/ Release(w)
    \/ \E k \in OKeys : Unbind(k) \/ Reconcile(k)
    \/ \E c \in Cids, g \in Gens : ClaimObj(c, g) \/ GcResumeObj(c, g)
    \/ UnlinkManifest \/ DeleteObjRow

Next == (NonCrashNext /\ UNCHANGED crashes) \/ Crash

Spec == Init /\ [][Next]_vars

Fairness ==
    /\ \A w \in Writers : WF_pvars(Materialize(w) \/ Bind(w) \/ Release(w))
    /\ WF_pvars(UnlinkManifest \/ DeleteObjRow)
    /\ \A c \in Cids, g \in Gens : SF_pvars(ClaimObj(c, g) \/ GcResumeObj(c, g))
    /\ \A k \in OKeys : SF_pvars(Reconcile(k))
FairSpec == Spec /\ Fairness

(* ----------------------- Invariants and properties --------------------- *)

\* SafeObj — a live name's manifest is always present.
SafeObj == \A k \in OKeys : nameRefs[k] > 0 => k \in manifests

\* I1 — at most one available generation per cid.
OneAvailable == \A c \in Cids : Cardinality(Avail(c)) <= 1

\* A referenced object-generation has its manifest.
LiveHasManifest == \A k \in OKeys : rows[k].refcount > 0 => k \in manifests

ClaimedHasNoRefs ==
    (gpc.phase \in {"claimed","unlinked"}) => rows[<<gpc.c, gpc.g>>].refcount = 0

RefcountDominates == \A k \in OKeys : rows[k].refcount >= nameRefs[k]

StateBound == \A k \in OKeys : rows[k].refcount <= MaxRef /\ nameRefs[k] <= MaxName

GCProgress == (gpc.phase # "idle") ~> (gpc.phase = "idle")
WriterProgress == \A w \in Writers : (wpc[w].phase # "idle") ~> (wpc[w].phase = "idle")
EventualReclaim ==
    \A k \in OKeys : (rows[k].exists /\ rows[k].refcount = 0)
                        ~> (~rows[k].exists \/ rows[k].refcount > 0)

=============================================================================
