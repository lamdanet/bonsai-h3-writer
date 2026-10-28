# H3 r2v prompt guide - images only (RTX 4060 8 GB laptop)

Short version of the [official reference-mode guide](https://huggingface.co/MiniMaxAI/MiniMax-H3/blob/main/docs/VIDEO_PROMPT_WRITING_GUIDE_ref_en.md)
for **image references only** (no reference video, no reference audio). Camera, shot and dialogue details:
[base guide](https://huggingface.co/MiniMaxAI/MiniMax-H3/blob/main/docs/VIDEO_PROMPT_WRITING_GUIDE_base_en.md).

## 1. Images in the workflow
- Up to **9 images**: Ref 1 ... Ref 9 = `<Picture 1>` ... `<Picture 9>` (the order you plug them in).
- Write everything in English. Keep spoken words in their own language inside `<d>`, and visible on-screen text exactly as shown.

## 2. Six sections, always in this order
```text
subject_definitions:
summary:
retention_analysis:
detailed_description:
overall_soundscape:
non_diegetic_music:
```
A label keeps the same meaning in every section once it is defined.

## 3. subject_definitions - one line per thing to track
**`<Subject N>`** = visible content that is reused in the video: a person, animal, object, room/landscape, outfit, prop, style, pose or expression.
- One subject can come from several images; one image can give several subjects.
- An image that only gives a look, a place or a style is **cited inside** the Subject line - no separate Picture line.

**`<Picture N>` gets its own line only when the image is a real frame or a plan:**
first frame, keyframe, last frame, edited keyframe, composition anchor, or a storyboard (say which shots it maps to).

| Your images | How to write it |
|---|---|
| 1 photo of a person | `<Subject 1> is the woman in <Picture 1>, <age, hair, face, figure, outfit>.` |
| several photos of the same person | `<Subject 1> is the woman whose appearance comes from <Picture 1>, <Picture 2> and <Picture 3>: ...` |
| different people | one Subject per person: `<Subject 1> ... <Picture 1>.` / `<Subject 2> ... <Picture 2>.` |
| a room or landscape | `<Subject 2> is the bedroom environment in <Picture 2>, with ...` |
| one photo gives person **and** place | two Subjects citing the same picture: person from `<Picture 2>`, environment in `<Picture 2>` |
| an outfit, prop or object | `<Subject 3> is the denim jacket worn by the man in <Picture 3>, with ...` |
| a style or mood | `<Subject 4> is the soft film look and warm grade of <Picture 4>.` |
| the exact first frame | `<Picture 1> is the first frame of [Shot 1], showing ...` |
| a keyframe / last frame | `<Picture 2> is the keyframe of [Shot 2] ...` / `<Picture 3> is the last frame of [Shot 3] ...` |
| a storyboard | `<Picture 5> is a storyboard reference for [Shot 1] and [Shot 2], defining viewpoint, placement and shot order.` |

## 4. summary - one short paragraph
Starts with the task type in brackets:
- `[reference generation]` - images guide look, place, style, outfit, pose or storyboard.
- `[keyframe completion]` - an image is a real frame (first, key or last frame).
- Both: `[reference generation + keyframe completion]` (join with ` + `, never repeat one).

Use only labels already defined above - no new labels here.

## 5. retention_analysis - one line per label
```text
<Subject 1> (appears in [Shot 1], [Shot 3]): fully_preserved - ...
<Picture 1> ([Shot 1] first frame): fully_preserved - ...
```
| Marker | Use when |
|---|---|
| `fully_preserved` | kept as defined |
| `partially_preserved` | still used, but some features change (new outfit, evening instead of day) |
| `attribute_transfer` | a feature moves to a different subject (his jacket on her) |
| `weak_reference` | only broad style, category, composition or mood is kept |

New actions, backgrounds or events you add are **not** losses - don't mark them down for that. No `(S1)` speaker IDs in this section.

## 6. detailed_description - the main part
- **350-500 words** (single shots too; spread detail by how much happens in each shot).
- **1-2 style sentences before `[Shot 1]`**: *The target video is a cinematic film scene with a 35mm look, soft grain and warm light.*
- Add *No subtitles, captions, text, watermarks or logos.* after the style sentence (this app's rule, not in the official guide: it keeps H3 from adding captions or logos; text that should be visible goes in double quotes).
- `[Shot 1]` has **no time**. Later shots: `[Shot 2] At 00:03.500, the camera cuts to ...` - times only go up and stay inside the video length.
  - Cut words: *the camera cuts to / the shot cuts to / transitions to / changes to / switches to*. Dissolve, fade or wipe only if you want one.
  - A cut must bring something new (subject, place, state, viewpoint, time). For a small change of distance or angle, move the camera instead.
- **Camera = type + optional amplitude + speed**, written inside the sentence: *the camera pushes in with small amplitude at slow speed*.
  Types: push in / pull out, zoom in / out, pan, truck, tilt, pedestal, arc shot, tracking shot, static shot, shake slightly/strongly, POV, roll.
- **Per shot describe**: framing, who and where (look + position), place and light, actions and changes, camera move, sounds.
- **Labels in the shots**: at a subject's first clear appearance give its look, position and action (*<Subject 1>, the woman from <Picture 1>, sits ...*); later just reuse the label.
- **Frame anchors**: *the shot begins from <Picture 1>* / *the shot's keyframe corresponds to <Picture 2>* / *the shot ends on <Picture 3>*.
- **Speech**:
  - `<Subject 1> (S1) says, <d>[English] Good morning.</d>` - a subject who speaks keeps both labels.
  - Someone without a picture: describe the voice, then the ID: *the young man with a warm, low voice (S2) says: <d>[English] ...</d>*.
  - IDs go in order of who speaks first and stay the same in every shot; people who never speak get none.
  - Voice-over: *says in an off-screen voiceover: <d>...</d> while her lips remain completely closed*.
  - Speech running over a cut: put `<scenetrans>` on both sides and say it *continues seamlessly across the cut*; speech cut off by the video end: `<cutoff>`.
  - Use exact words; `[unclear]` for unclear words; end lines with `.`, `?` or `!`.
- **On-screen text**: in double quotes, exactly as written: *a sign reading "OPEN"*.

## 7. overall_soundscape / non_diegetic_music
- `overall_soundscape:` 1-4 sentences of ambience and physical sounds (room tone, footsteps, rain, breathing). No dialogue here. `N/A` only for total silence.
- `non_diegetic_music:` 1-3 sentences: instruments, tempo, rhythm and how loud it gets - no mood words. `N/A` if none. Music the characters can hear goes in the description instead.

## 8. Template
```text
subject_definitions:
<Subject 1> is the woman whose appearance comes from <Picture 1> and <Picture 2>: <look>.
<Subject 2> is the <place> environment in <Picture 2>, with <key features>.

summary:
[reference generation] The target video shows <Subject 1> <main action> in <Subject 2> in <shots / one continuous take>.

retention_analysis:
<Subject 1> (appears in [Shot 1]): fully_preserved - <what is kept>.
<Subject 2> (appears in [Shot 1]): fully_preserved - <what is kept>.

detailed_description:
The target video is <style, light, colour, lens>. No subtitles, captions, text, watermarks or logos.
[Shot 1] <framing>. <Subject 1>, the woman from <Picture 1> and <Picture 2>, <look, position, action>. <place and light>. The camera <move> with small amplitude at slow speed. <what happens next>. <ending>.

overall_soundscape: <ambience and physical sounds>.

non_diegetic_music: <instruments, tempo, dynamics> / N/A
```

## 9. Laptop settings (8 GB VRAM, 16 GB RAM)
- `ref_image_size` = **match** (never "max": one photo would cost more than the whole video).
- Same person: **2-4 photos** from different angles, one outfit (or say which outfit to use). 5-9 only for several people/things.
- Tests **0.2 MP, 3-4 s**. Finals: 6 s up to 0.5 MP, 10 s up to 0.4 MP, 15 s at 0.2-0.3 MP.
- H3 rounds the length: 3 s = 3.04, 4 s = 4.46, 5 s = 5.17, 6 s = 6.58, 8 s = 8.00, 10 s = 10.13, 12 s = 12.25, 15 s = 15.08, 20 s = 20.04 (the request builder writes it next to the length).
- One continuous action per shot; 1 shot for 3-6 s, about one cut per 3 s at most.
- **First-frame pin** (bypassed group in the workflow) pins **Ref 1** as frame 1 - turn it on only when Ref 1 is the real first frame, and write `<Picture 1> is the first frame of [Shot 1]` + `[keyframe completion]`.
- Crop watermarks off photos first. One render at a time, 4-5 min cool-down, fans on Max.
