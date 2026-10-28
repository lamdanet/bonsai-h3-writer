# How to write the f2v prompt (H3 first frame = I2VA / FL2VA)

Official guide: [Base guide (T2VA / I2VA / FL2VA / L2VA)](https://huggingface.co/MiniMaxAI/MiniMax-H3/blob/main/docs/VIDEO_PROMPT_WRITING_GUIDE_base_en.md)  
Reference-mode guide (for r2v): [Reference mode guide](https://huggingface.co/MiniMaxAI/MiniMax-H3/blob/main/docs/VIDEO_PROMPT_WRITING_GUIDE_ref_en.md)

**Line 1 (always first, then one empty line):**

First frame only (Ref 1 on, Last frame bypassed) = **I2VA**:
`For the target video, at 0.00 seconds into the target video, <Picture 1> (from [Shot 1]) is fully referenced.`

First + last frame (Last frame switched on) = **FL2VA**:
`How the reference pictures align with the target video — Picture 1 (from Shot 1) aligns with the 0.00-second mark of the target video; Picture 2 (from Shot 1) aligns with the 3.04-second mark of the target video.`
(use the real length H3 makes: 3 s -> 3.04, 4 s -> 4.46, 5 s -> 5.17, 6 s -> 6.58, 8 s -> 8.00, 10 s -> 10.13, 12 s -> 12.25, 15 s -> 15.08, 20 s -> 20.04)

**Then three fields:**

`integrated_multimodal_description: [Shot 1] Live-action, cinematic, <framing> ...`
- Start from the first frame (who, where, pose, framing), then: **action onset -> continuous development -> result**. Keep face, clothes, colours and layout as in the image.
- With a last frame: describe the **path** between the two images, not the two images again, and end on *settles into the pose and composition of Picture 2*.
- **One shot** is best for f2v. More shots: `[Shot 2] At 00:03.000, the camera cuts to ...` (first shot has no time).
- Camera inside the sentence: *the camera pulls out at slow speed in front of her as she walks*.
- Speech: `the woman (S1) says: <d>[English] Hello.</d>`
- End the field with *No subtitles, captions, text, watermarks or logos.* (this app's rule, not in the official guide: it keeps H3 from adding captions or logos).

`overall_soundscape:` 1-4 sentences of ambience and physical sounds (`N/A` only for total silence).

`non_diegetic_music:` instruments, tempo, dynamics, or `N/A`.

**Size:** set the aspect to match the first-frame image - f2v stretches it to the video shape.

### Jiggle tips (from our tests)
- **One continuous action per shot** (e.g. walks the whole time). Stopping or switching action mid-shot looked like sudden shaking.
- **Tie the jiggle to the movement**: *her chest bounces in time with each footstep and settles between steps*.
- Bouncing LoRA: keep both triggers - *her breast is bouncing up and down, her breast is bouncing from left to right*. Slop LoRA: *her breasts bounce, shake, jiggle and sway*. The body action sets the direction.
- Add *her body keeps exactly the same size and proportions* and describe the clothes clearly.
