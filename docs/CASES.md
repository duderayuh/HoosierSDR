# Case timelines — cardiac arrest first

Status: **proposal**, 2026-09-14. Nothing in this document is built yet. It
is the design conversation the request asked for, written down so the next
session starts from it rather than from the codebase again.

## The short version

- Most of the pipeline this needs already exists: dispatches become incidents,
  hospital reports are summarised and joined back to the run by the callsign
  the crew says, drive times come from a local router, a map picture goes to
  Telegram, and boards are already served to a wall display over the tailnet.
- What is missing is a **timeline on top of an incident** (a "case" with
  events and a state), a **radio identity layer** with evidence behind it, a
  **predicted arrival window** instead of an ETA phrase, and a **Telegram
  cadence that edits one message** instead of sending many.
- Build it as a generic *case* with a *profile*, cardiac arrest being the first
  profile. The repo's own rule is no county's talkgroups or names in the code;
  the dispatch talkgroup, its backup, the ops talkgroups and the hospital
  channels already have homes in Dispatch setup and the place book, and the
  profile refers to those roles, not the numbers.
- Three honest pushbacks, detailed below: low-flow time cannot come off the
  radio; the arrival prediction must be anchored on the crew's report, not the
  scene; and the timestamps are the trustworthy part while the labels are the
  fragile part, so every line of a timeline must cite the call it came from.

## What already exists

| Need | Where it is today |
|---|---|
| Dispatch call → incident with address, units, call type, emoji, position | `dispatch.rs`. Grouping by normalised address key, then within 150 m with agreeing type, then by shared unit for tactical channels (`pick_target`). |
| Hospital report → hand-off note with headline and stated ETA | `conversations.rs`. The ETA phrase is lifted out on read (`eta_phrase`), from a pattern fitted to 288 real summaries. |
| Hospital report joined to the run it came from | `link.rs`. Spoken callsign, 75 min back / 5 min ahead; rules decide, the model breaks ties. On one real library: 60 reports, 16 joined, 23 named no crew, 21 named a crew whose dispatch was never recorded. |
| Distance and drive time to a hospital | `pathways.rs` + `routing.rs` (OSRM in a container, straight line as the honest fallback), stored on `incidents.targets`. |
| Map picture to Telegram | `mapshot.rs`, drawn in Rust from OSM tiles, works with the lid shut. |
| Telegram threads, replies, edit-in-place, HTML with plain fallback | `alerts.rs` (`send_text_reply_html`, `edit_message`, `delete_message`), tripwire `Thread`s. |
| Boards on the desktop and on the tailnet | `dashboards.rs`, two pane kinds (`dispatch`, `reports`), matched in Rust, keyed share links. |
| Radio ID names | `units.rs`: per-system aliases, regex rules, over-the-air talker aliases. `conversations.rs` learns the hospital's fixed console IDs (≥3 seen and ≥60 %). |
| Arrest extraction | `analyzers.rs` built-in screens: CPR in progress, outcome (ROSC / terminated), ECPR candidate. Per transmission, sent to Telegram, **nothing written back to the incident**. |

The honest summary of the gap: the app knows *that* a run exists and *where*
the patient went. It does not know *what happened in between*, and it has no
memory of a run's state.

## What is missing

1. **A case.** An incident has one `call_type`, one `summary` and a revision
   counter. There is no ordered log of events, no state (dispatched → working →
   ROSC → re-arrest → terminated / transporting → arrived), and no downgrade
   ("this is an overdose, not an arrest").
2. **Linking the ops-channel traffic reliably.** An ops-channel call attaches to an
   incident only when the extraction model pulls a unit callsign the incident
   already has. Most upgrades do name the unit ("Medic 32, upgrade to a
   working arrest"), so this mostly works, but the transcriber writing
   "medic thirty two" or "medic 3 2" loses it. The radio ID is a second key
   that does not depend on the transcript.
3. **A radio identity layer with evidence**: who is this radio, since when,
   how sure, and why.
4. **Predicted arrival as a time window**, not a phrase in prose.
5. **A Telegram cadence** for a run that changes six times in forty minutes.
6. **A board pane** for cases, with the map image, on the shared board too.

## Design

### 1. Cases and profiles

Two new tables, additive, alongside `incidents`:

```
cases:        id, incident, profile, state, opened, updated, closed,
              hospital_place, arrival_anchor, arrival_lo, arrival_hi,
              arrived_at, chat, root_message, map_path, facts (JSON)
case_events:  id, case, at, kind, label, call, conversation,
              source (rule | model | manual | system), confidence, detail (JSON)
```

`facts` holds the things the ED asks about, each with the call it came from:
witnessed, bystander CPR, initial rhythm, age, sex, comorbidities, downtime
*as stated by the crew*. Absent means "not stated", never "no".

A **profile** is configuration (seeded with cardiac arrest, editable, and the
template for stroke / STEMI / trauma later):

- *opens on*: dispatch call types (`Cardiac Arrest`) **or** escalation phrases
  on tactical channels (`working arrest`, `CPR in progress`, `working code`…)
- *event vocabulary*: phrase → event kind (`upgrade`, `rosc`, `re_arrest`,
  `terminated`, `transporting`, `arrived`, `downgrade`, `cancelled`)
- *closes*: N minutes after the hospital report, or on `arrived` /
  `terminated` / `cancelled`

This is how "the original dispatch could be an unconscious person" falls out:
the case opens at the *upgrade* on the ops channel, and the timeline backfills the
dispatch line from the incident the upgrade linked to. Nothing opens a case
for every unconscious person.

### 2. Linking: three keys, in a fixed order

1. **Address key**, on dispatch channels (exists).
2. **Spoken callsign**, using the vocabulary `link.rs` already learns from the
   county's own dispatches. Today only hospital reports use it; the same cheap
   rule runs on tactical traffic before any model is asked.
3. **Radio ID** (new). If the transmitting radio has an accepted identity
   "Medic 32" valid at that time, the call is treated as if it said "Medic 32".
   And any radio that spoke in the linked hospital conversation is bound to
   the case for its lifetime, so a later "we lost pulses" from that radio on
   the ops channel attaches with no callsign said at all.

Rules decide and the model breaks ties, exactly as `link.rs` does. Two open
arrest cases are never merged on a unit alone; `dispatch.rs` already learned
that one engine runs many calls inside one window.

### 3. Reading events off calls

- A **phrase gate** per profile runs first. The tripwire preview already
  reports which phrases are actually said on a channel and what the
  transcriber writes instead; that is the tool for tuning the vocabulary.
- Calls that pass the gate get **one** extraction call: event kind, ETA low /
  high / exact, hospital, witnessed, bystander CPR, rhythm, ROSC, comorbidities,
  with nulls. No model call for calls the gate refuses.
- Every event carries its call id, the matched phrase and a confidence.
  Nothing appears on a timeline without a source that can be played.
- The hospital report already goes through the model once for the headline
  and note; the case facts are asked for in that same call, the way the
  headline was added, so a report costs no second read.

### 4. Times: what the radio can honestly say

| Wanted | What the radio gives | What the message says |
|---|---|---|
| Low-flow time | Nothing about the collapse. Dispatch time is the earliest fact. | "Since dispatch: 39 min" and "CPR first reported 06:12". If the crew *states* a downtime, quote it as theirs. The label "low-flow" never appears on a number the app worked out. |
| Witnessed, bystander CPR, comorbidities | Only if said, usually in the hospital report. | yes / no / not stated. Default is not stated. |
| ROSC, re-arrest | Said on the ops channel or in the report. | "ROSC reported 06:23 (11 min after CPR first reported)". |

An ED that anchors on a wrong number is worse off than one with no number.
The value here is that the timestamps come from the radio's clock, not from a
crew doing compressions; the app should not spend that credibility on
inferences.

### 5. Predicted arrival

- **Anchor** = the start of the transmission in which the crew gave the ETA.
  That clock is reliable.
- **Stated ETA** parsed to minutes (a range "5 to 7", a single number, or a
  clock time "at 06:40"), extending `eta_phrase` from a highlight into numbers.
- **Road time** from the scene to the hospital that owns the talkgroup they
  called (place book → OSRM). It is an upper bound only until they leave, and
  stale afterwards, so it is used two ways: a sanity check (a stated ETA much
  longer than the road time from scene is flagged, not corrected), and the
  fallback when nothing was stated ("≤ 9 min by road from scene, departure
  time unknown").
- **Output**: a window, `anchor + lo` to `anchor + hi`, replaced by each new
  ETA. Rendered as "said 5–7 min at 06:32 → 06:37–06:39".
- **Closing the loop**: an "arrived" / "at the hospital" call on the ops channel
  records the actual arrival, and the predicted-versus-actual error is kept
  per unit. That error, over weeks, is the only thing that could ever make the
  prediction better than restating what the crew said. This is not a model. It
  is a clock and arithmetic, and it should be described that way to the ED.

### 6. Radio identity (the "UID tracker")

A new table, not another JSON file, because it will hold thousands of rows:

```
radio_evidence: system, radio, callsign, role (unit | console | hospital),
                how (said_self | addressed | reply | dispatch_list |
                     conversation_fixed | manual | model),
                call, at, weight
```

Rules over dispatch-channel transcripts, using the learned callsign vocabulary:

- "control, medic 32" / "medic 32 to control" → the speaker is Medic 32
  (strong).
- "medic 32 from control" → the speaker is a **console** (strong), and the
  first non-console radio to reply within ~20 s is Medic 32 (medium).
- A tone-out listing units → radios keying up within ~90 s that are not
  consoles are one of those units (weak; resolved once a strong hit lands).
- The hospital channel already learns fixed IDs; they become role `hospital`.

An identity is per (system, radio): a histogram of callsigns with first and
last seen. It is accepted at ≥3 pieces of evidence and ≥60 % agreement, the
threshold the conversation engine already uses for consoles, unless you pin
it. Portables move between trucks and people, so a callsign has a validity
window; a conflicting run of evidence closes the old window and opens a new
one, and the history stays visible.

UI: Settings → Radio IDs gains a *Learned* list (accept / reject / pin, with
the evidence behind each). Right-click on any call row: "This radio is…"
(a unit, a console, a hospital). The same on each participant in Conversation
details. A backfill runs the rules over the whole library in seconds; the
model is used only for radios that have evidence but no strong rule hit,
asked per radio with a closed list of candidates, the same shape as the
tie-break in `link.rs` (an answer off the list is discarded).

### 7. Telegram cadence

Recommendation: **one thread per case, and the root message is the timeline,
edited in place.** Edits are silent; nobody's phone buzzes. A reply, which
does notify, goes out only for a fixed list of events: case opened, ROSC,
re-arrest, hospital report with ETA, an ETA change of three minutes or more,
downgrade / cancel, closed. The map picture is sent once, as a reply to the
hospital report, drawn scene → hospital; at dispatch it would only change.

A bad night's arrest is then about six notifications over forty minutes; an
ordinary one, three. The root is edited after every event, so someone who
opens the thread late sees the whole story in one message.

Two things this needs that the tripwire threads do not have: the root
message id must be **persisted** on the case (tripwire threads live in memory
and a restart loses them), and Telegram's 48-hour edit limit is fine for a
case that lives an hour.

The root message, with the anchors the ED can trust:

```
🫀 Working arrest · 1200 Example St · Medic 32, Engine 6
06:01  dispatched as Unconscious Person
06:12  upgraded to working arrest · Medic 32
06:23  ROSC reported
06:27  report to General · said ETA 10 min → arrives 06:37
06:32  lost pulses
06:40  re-report · said ETA 7 min → arrives 06:47
Since dispatch 39 min · CPR first reported 06:12 · ROSC 06:23
Witnessed: not stated · Bystander CPR: not stated · Comorbidities: not stated
```

Which chat: a place in the place book can carry its own Telegram destination,
so a report to one hospital goes to that hospital's chat, with the default chat as
the fallback. Whether you want that or one chat is a question below.

**Nothing is sent until it has been replayed.** A "replay over the library"
renders the timelines the profile would have produced from the last N days
and counts the messages per day, the same idea as the tripwire preview. You
read those before the first live send.

### 8. Board

A third pane kind, `cases`: a timeline card per open case with a state stripe
(working / ROSC / transporting), the arrival window as a countdown, the facts
line, and the map thumbnail. The PNG is saved under the library and served
through the board's own keyed route so a shared display gets it too. Matching
stays in Rust, as the boards already insist.

## Risks and pushbacks

1. **The transcriber.** Radio ASR runs near 50 % word error; "lost pulses" and
   "got pulses" differ by a phoneme. Rules will misfire and a model will fill
   gaps with fiction. Mitigations: every event cites its call; confidence is
   shown; an event is never deleted silently, only marked; the existing
   transcript edit corrects the source; and the Whisper vocabulary prompt
   ("tell Whisper what a dispatcher says") gets the arrest vernacular.
2. **Radio identity is not a table, it is a history.** Portables versus
   mobiles, spares, crews rotating. Evidence with validity windows, and the UI
   must show why the app believes what it believes.
3. **"Predicted arrival" is the crew's number restated on a reliable clock.**
   That is worth having, but call it what it is. The genuine improvement is
   the per-unit error log, and it takes weeks of arrivals to mean anything.
4. **No model budget.** Each tripwire firing spawns a thread and there is no
   cap on concurrent model calls. Gating tactical channels through the model
   on a busy night would pile up. The phrase gate handles most of it; a small
   concurrency cap on model calls is worth adding regardless.
5. **Audience.** The traffic is public, but the messages will carry age, sex,
   condition and address into a hospital group chat. The destination should
   be a closed group, and the app already scopes destinations per rule.
6. **One county's flow.** The profile is data. Stroke, STEMI and trauma are a
   second JSON, not a second feature.

## Questions

Answers to these change what gets built, in the order they matter.

1. In Dispatch setup today, are the ops talkgroups marked as **tactical**
   channels, and do the hospital talkgroups sit on their hospitals in the
   place book? If not, the linking that already exists is not running on your
   library, and that is the first thing to fix.
2. Does an arrest always start with a dispatch on the dispatch talkgroup or its backup, or can a crew
   upgrade a run that has no dispatch in the library (mutual aid, a channel
   you do not record)? This decides whether a bare "working arrest" on an ops channel
   opens a case on its own.
3. Do crews call "transporting", "en route to <hospital>", or "arrived" on the ops channels?
   Arrival is what makes the ETA error measurable.
4. Telegram audience: one chat (yours, at one hospital), or a chat per hospital
   routed by which hospital the crew called?
5. Cadence: is "one edited timeline plus a notification on those seven event
   kinds" right, or do you want every event as its own reply?
6. Send the dispatch console radio IDs, the hospital radio IDs you
   know, and the system they are on. They seed the identity table and let the
   "medic 32 from control" rule work from day one.
7. Should a run dispatched as a cardiac arrest that is never upgraded and
   never produces a hospital report open a Telegram thread at all? Many are
   cancelled or not transported.
8. What are the actual words on the ops channels for upgrade and downgrade? "Working
   arrest", "working code", "upgrade to a working", "this is going to be an
   overdose"… The dead-phrase tool can verify them against the library, but
   only you know the vernacular.
9. Are you content with the generic case-plus-profile framing, with cardiac
   arrest as the first profile, rather than an arrest-only feature?

## Build order

Each step ships on its own, is replayable over the library, and sends nothing
to Telegram until its preview has been read.

1. **Radio identity**: evidence table, rules, backfill, Settings panel,
   right-click. Useful alone; it strengthens `link.rs` immediately.
2. **Cases**: profile, event reading, replay over the library, a Cases tab
   with the timeline view. No sending.
3. **Arrival window**: parsed ETA, road-time check, arrival matching, error log.
4. **Telegram**: edited root, replies on the event list, message-count preview.
5. **Board pane** with the map thumbnail, on shared boards.

All schema changes are additive (new tables, one nullable column on
`incidents` at most), so an older build still opens the library.

## Findings from the library

Measured 2026-09-14 on the live library, read-only: 67 hours, 18,472 calls,
1,555 incidents, 334 hospital reports of which 105 are joined to a run. The
library is short because an old retention setting deleted every unstarred call
at each start until 2026-09-11; cleanup is off now, so it will grow.

Talkgroup numbers, hospital names and radio IDs are kept out of this file on
purpose. Roles below: *dispatch* is the tone-out talkgroup, *ops* the two
conversational channels between dispatch and the units, *consoles* the
dispatch centre's own radios.

### The answers that were in the data

**Question 1: the ops channels are not configured.** Only the dispatch
talkgroup is marked in Dispatch setup. The two ops talkgroups carry no role, so
the tactical path in `pick_target` has never run on them, and an upgrade said
there reaches no incident. The backup dispatch talkgroup has no calls in the
library. Every hospital talkgroup is on its hospital in the place book. This is
a settings change for the listener, not code.

**Question 3: transport is announced, arrival barely.** Crews say
"transporting, emergent" and name the hospital, and the console reads it back.
"At the hospital" appears mostly as the dispatcher asking for a status.
Arrival closure is feasible for some runs, not most.

**Question 8: the vernacular, counted on the two ops channels.**

| Phrase | Ops calls in 67 h | Note |
|---|---|---|
| working (cardiac) arrest | 10 | crew request, then console readback |
| not a cardiac arrest / not an arrest | 7 | the downgrade; often "overdose, not a cardiac arrest" |
| transport(ing) | 24 | mostly requests for a second transport unit |
| DOA | 6 | "this is going to be a DOA", then a time asked for |
| ceasing efforts | 2 | transcribed once as "A ceasing effort to hate" |
| ROSC | 2 | one crew call and its readback |
| upgrade | 4 | "can you upgrade this to a working cardiac arrest" |
| CPR, compressions, pulses, lost pulses, v-fib, asystole | 0 | never said on ops in this window |

So a timeline built from the ops channels gets *working*, *downgrade*,
*transporting*, *DOA / ceasing efforts* and occasionally *ROSC*. Pulses lost
and regained are said to the hospital, if at all, not to dispatch.

**Question 6, partly: consoles identify themselves.** One console radio carries
the dispatch talkgroup almost alone. Five others answer on the ops channels, and
a rule for "medic 32 from control" found exactly those five and nothing else.

### What the data changes in the design

1. **The console readback is the event.** Every crew status on ops is read back
   by a console within about five seconds, with a clock time: "Working Arrest
   1748", "rosc 1914", "Ceasing efforts 2326", "Not a cardiac arrest, 1714",
   "Transporting <hospital>, … 663". The speaker is a known radio, the format is
   fixed, the speech is cleaner than a crew on scene, and the time is the one
   the dispatch centre logged. The design now takes the readback as the
   canonical event and the crew's request as supporting evidence, and it uses
   the spoken time as a second clock to check the call's start against.
2. **The radio ID is a weaker key on ops than assumed.** 14–15 % of transcribed
   ops calls have no radio ID, against 1.4 % on dispatch, and 4 of the 11 arrest
   upgrade or downgrade calls are among them. These calls have speech, so they
   are not the announcement rows fixed on 2026-09-14. The ops channels are
   granted through Motorola regroup grants; if a call is opened by the update
   form, which names no radio, only a confirmed Link Control word can. Recorded
   here as a decoder question, not part of this work.
3. **The same-radio bridge rarely reaches the hospital.** Arrests are mostly
   marked by the engine crew, and the engine does not call the hospital; the
   medic does. Of the arrest calls from a named radio, one (perhaps two) went on to
   report to a hospital. The chain is therefore upgrade → incident (by console
   readback time, callsign, or radio identity) and incident → hospital report
   (by the existing callsign join), not upgrade → report directly.
4. **Time alone links about half.** For the 11 ops arrest calls, the dispatch
   incidents open in the previous 30 minutes were: exactly one Cardiac Arrest
   run in 5 cases, none in 3 (the run had been dispatched as something else),
   two or more in 3. A bare "can you upgrade this to a working cardiac arrest"
   with no radio ID and no callsign had ten runs open and none of them an
   arrest; no key reaches it, and it goes to the tie-break or stays unlinked.
   Where a radio was named, identity helped: the radio that said "working
   arrest" at one run had self-identified four times as the medic on that run.
5. **Fuzzy matching is not optional.** "Oregon arrest 1157", "receiving
   efforts", "not under arrest", "A ceasing effort to hate". The profile
   vocabulary goes through `fuzzy.rs` the way tripwire phrases already do.
6. **Radio identity grows with time.** A self-identification rule
   ("control, medic 32", "medic 32 to control", "control from engine 44") over
   6,553 transcribed dispatch and ops calls gave some identification for 156 of
   388 field radios and an accepted one, at ≥3 hits and ≥60 % agreement, for
   19. Its disagreements were misheard digits (24 and 44, 54 and 64), which the
   majority absorbs. In 67 hours that is thin; it is not thin after a month.
7. **The listener's arrest tripwires are what cases replace.** A folder of
   step tripwires already does this by hand: arrest chatter on ops, working
   arrest on dispatch, updates on ops, the report on each hospital channel with
   follow-ups by radio, and an ECPR screen. When cases send to Telegram, that
   folder is migrated or switched off, or every arrest is announced twice.

### Build order, revised

0. **Listener:** mark the two ops talkgroups as tactical in Dispatch setup, so
   the existing unit matching runs on them while the rest is built.
1. **Radio identity**, with console detection first, because the readback rule
   needs to know which radios are consoles.
2. **Cases**, reading events from console readbacks, replayed over the library.
3. **Arrival window.**
4. **Telegram**, replacing the step tripwires.
5. **Board pane.**

## The listener's answers (2026-09-14)

1. **Arrests start under other names.** A run can be dispatched as unconscious
   person, difficulty breathing or something else and become an arrest later.
   It can also skip straight to "Cardiac Arrest Working" (transcribed as
   "cardiac arrest, working" or with a typo). Pediatric arrests follow the same
   pattern, and a pediatric arrest almost always goes to the closest children's
   hospital, which the existing pediatric-arrest pathway already expresses.
2. **One Telegram chat per hospital.** A place gains its own destination,
   additive; the default chat is the fallback.
3. **Cadence agreed:** one edited timeline, notifications on the key events.
4. **Every arrest dispatch opens a case**, working or not.
5. **Radio IDs are rarely named by the system.** Identity comes from what is
   said: units calling the hospital, the ops channels, and the automated
   dispatch voice, which names every unit sent to an address.

Further facts from the listener, checked against the library:

- **The dispatch talkgroup is an automated voice.** A page reads: units,
  address, call type, then the same again, then "<time> Hours, Location <grid>",
  sometimes "Assigned to Op N". One radio carries it.
- **A run is repaged as it grows, and the repage carries the upgrade.** One
  arrest was paged five times in nine minutes, each page naming the units
  added, and the last two as "Cardiac Arrest Working". Because the page repeats
  the address, the address key links the upgrade to its run with no callsign
  and no radio ID. The upgrade now has three sources, ranked by how clean they
  are: the automated repage on dispatch, the console readback on ops, and the
  crew's own request on ops. A bare "can you upgrade this" is usually followed
  by a repage that does carry a key.
- **Units swap runs.** "Control, EMS 93, I can take that from 91, I'm closer",
  and the run is repaged with the new unit. The incident's unit list is a union,
  so both stay on it; harmless for linking, but a case shows the swap.
- **"Refer to MDT for units."** Some pages name no units. A hospital report
  about a very similar complaint shortly after is probably that run, but no
  rule can prove it, so the join gains a third outcome, *inferred*, which is
  always labelled as such wherever it is shown or sent.

Two gaps seen while checking:

- `tidy()` snaps "Cardiac Arrest Working" to the configured "Cardiac Arrest",
  so the qualifier is lost from the incident's type and survives only in the
  transcript and the stored extraction. The case engine reads the dispatch
  transcripts for it rather than the incident's type.
- **A repage can fork the run.** One page was transcribed without its house
  number, its key became the bare street, the geocode landed on the street's
  centre beyond the 150 m radius, and a second incident opened. A case built on
  incidents must merge these: same street, compatible type, a shared or added
  unit, within minutes.

**What "generic case" and "readback as event" mean.** A *case* is one timeline
mechanism, and a *profile* is a settings file that tells it which call types
open a case and which phrases are events. Cardiac arrest is the first profile;
stroke or trauma later is a second profile, not new code. *Readback as event*
means that when a crew's garbled "can you upgrade this to a working arrest" is
followed by the dispatcher's clean "Working Arrest 1748", the timeline records
the dispatcher's version and time, with the crew's call kept as supporting
evidence.

## Step 1 built: radio identity (2026-09-14)

`app/src/radios.rs`. Evidence per call in `radio_evidence`, the listener's
word in `radio_verdicts`, identities folded on read. On a copy of the live
library: 270 radios with evidence, 50 units learned, 4 consoles, the page
voice, 13 hospital radios; 59 more hospital reports joined to their runs
through the radio that called (116 → 175).

Known gaps, deliberately left:

- **A reply's transcript can land before the console's.** Live, the reply
  rule then sees a console call with no text yet and writes nothing. A sweep
  every 10 minutes re-reads the last 6 hours in order and catches it.
- **Identity has no time window yet.** A learned callsign applies to every
  call from that radio, old or new. "Changed" catches a radio that moves to
  another crew, but only after three sightings.
- **More joins means more incident tripwires fire.** A join made through a
  learned radio counts as linked, so a tripwire waiting for the hospital
  report fires for those runs too.

**Retrying joins (added the same day).** A report tries to join its run when
it is stored, and its radio may not be learned yet. So a report is tried
again when any radio in it becomes learned or is confirmed (the model may
break a tie, once), and every 10 minutes all unjoined reports from the last
6 hours are tried by rules alone. A join made more than 20 minutes after the
report ended is recorded and shown, but tells no tripwire, so nothing reaches
Telegram after the patient has arrived. Two retries reaching the same report
cannot both join it: the link is written only where none exists, and only
the retry that wrote it announces it.
