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
  10202 / 10244 / 49F / the hospital channels already have homes in Dispatch
  setup and the place book, and the profile refers to those roles, not the
  numbers.
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
2. **Linking the ops-channel traffic reliably.** A 49F call attaches to an
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
the case opens at the *upgrade* on 49F, and the timeline backfills the
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
- **Closing the loop**: an "arrived" / "at Methodist" call on the ops channel
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
🫀 Working arrest · 4350 Madison Ave · Medic 32, Engine 6
06:01  dispatched as Unconscious Person
06:12  upgraded to working arrest · Medic 32
06:23  ROSC reported
06:27  report to Methodist · said ETA 10 min → arrives 06:37
06:32  lost pulses
06:40  re-report · said ETA 7 min → arrives 06:47
Since dispatch 39 min · CPR first reported 06:12 · ROSC 06:23
Witnessed: not stated · Bystander CPR: not stated · Comorbidities: not stated
```

Which chat: a place in the place book can carry its own Telegram destination,
so a report to Methodist goes to Methodist's chat, with the default chat as
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

1. In Dispatch setup today, are 49F North and South marked as **tactical**
   channels, and do the hospital talkgroups sit on their hospitals in the
   place book? If not, the linking that already exists is not running on your
   library, and that is the first thing to fix.
2. Does an arrest always start with a dispatch on 10202 / 10244, or can a crew
   upgrade a run that has no dispatch in the library (mutual aid, a channel
   you do not record)? This decides whether a bare "working arrest" on 49F
   opens a case on its own.
3. Do crews call "transporting", "en route to Methodist", or "arrived" on 49F?
   Arrival is what makes the ETA error measurable.
4. Telegram audience: one chat (yours, at one hospital), or a chat per hospital
   routed by which hospital the crew called?
5. Cadence: is "one edited timeline plus a notification on those seven event
   kinds" right, or do you want every event as its own reply?
6. Send the console radio IDs (the 7900xx ones), the hospital radio IDs you
   know, and the system they are on. They seed the identity table and let the
   "medic 32 from control" rule work from day one.
7. Should a run dispatched as a cardiac arrest that is never upgraded and
   never produces a hospital report open a Telegram thread at all? Many are
   cancelled or not transported.
8. What are the actual words on 49F for upgrade and downgrade? "Working
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
