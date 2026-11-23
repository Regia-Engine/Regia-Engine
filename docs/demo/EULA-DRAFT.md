# Regia Engine showcase demo: end user licence agreement

**DRAFT for the owner. Not in force. Not legal advice; counsel reviews it before the demo ships.**
Proposed 2026-11-06 (ADR-3111). The owner approves the text and fills the bracketed points.

> **Status: superseded in part by MPL-2.0 (ADR-10225).** This draft was written for the earlier proprietary engine
> licence. The engine code in the Demo is now under the Mozilla Public License 2.0 (section 2.4), so this text can
> cover only the Demo's own scene, content and program as a whole; whether the Demo needs an end user licence at
> all is the owner's call when it ships (ADR-10254).

Licensor: not filled in (this draft predates the MPL-2.0 switch; see the status note). Brackets `[...]` are
decisions or facts the owner supplies.

## 1. What this is

1.1 This agreement covers the free showcase demo of Regia Engine (the "Demo"): the program, the scene and the
sample content it ships with, and the documentation in its download. It is a technology demonstration, not a game
and not a finished product.
1.2 You accept by installing, running or copying the Demo. If you do not accept, delete it.
1.3 The Demo is free. Nothing in it is for sale, and nobody may sell it (section 2.3).

## 2. Your rights

2.1 We grant you a worldwide, non-exclusive, non-transferable, revocable licence to install and run the Demo on
computers you own or control, for personal, non-commercial evaluation.
2.2 You may copy the unmodified Demo download to give to other people **[owner: yes/no; proposed yes, only as the
complete unmodified download with this agreement and the notices file]**.
2.3 You may not sell, rent, lease or charge for access to the Demo; remove or alter notices; reverse engineer the
Demo's executable code except where the law allows it despite this clause; or extract the Demo's assets for use in
another work, except the video and image uses in section 3.
2.4 The Demo contains compiled Regia Engine code. Regia Engine itself is licensed under the Mozilla Public License
2.0 (`licenses/LICENSE` in the download); that licence, not this agreement, governs the Regia Engine code, and its
source code is available as stated in the download's `CREDITS.txt`. This agreement covers the Demo's own scene,
content and program as a whole. Nothing here grants a licence to a trademark except to name the Demo accurately
(see the Regia Engine trademark policy).
2.5 We and our licensors keep all rights not granted here.

## 3. Videos, streaming and screenshots

**[Owner decision, proposed text below. Alternatives in "Open points for the owner".]**

3.1 You may record, stream and publish videos and screenshots of the Demo, on any platform, with or without
advertising or other income that you earn from your channel, provided that the video or image does not present the
Demo as something other than what it is and does not imply that we endorse you, your channel or a product.
3.2 Please name the Demo and link to its download page.
3.3 This section covers footage and images of the running Demo. It does not let you publish the Demo's files, source
data or extracted assets.
3.4 We may ask you to remove content that breaks this agreement or the law. We will say why.
3.5 Any third-party asset in the Demo may carry its own credit line; the credits page and the notices file list
them. **[Pending the content pack: until it is delivered, the Demo ships only content made by Regia Engine itself.]**

## 4. Privacy

4.1 The Demo works offline. It has no accounts, no analytics, no advertising and no telemetry upload, and it does
not connect to the internet. **[Owner decision: none (proposed), or opt-in only. If opt-in is chosen, this section
is rewritten to say what is sent, to whom and how to turn it off, and the Demo asks on first launch with the answer
off.]**
4.2 If the Demo stops unexpectedly it can write a crash report file on your computer. The Demo does not send it
anywhere. Nothing is uploaded unless you attach the file to a message yourself.
4.3 The Demo stores its settings, screenshots and frame-time summaries in a folder under your user account
[path per platform: owner/build lane to fill]. You can delete that folder at any time.
4.4 Platform stores and operating systems collect data under their own terms, which this agreement does not change.
**[If the Demo is distributed through a store, the store page's privacy fields must match this section.]**

## 5. Third-party software

5.1 The Demo includes open-source and other third-party software under its own licences. They are listed with their
licences and copyright notices in `THIRD_PARTY_NOTICES.md` in the download. Those licences apply to those
components, not to the rest of the Demo.
5.2 Where such a licence gives you more rights than this agreement for a component, you have those rights for that
component.
5.3 Upscaling: the Demo ships the in-tree AMD FidelityFX Super Resolution 3 based upscaler (MIT licence, notice in
the notices file). **[If a vendor upscaler runtime is added, its required attribution text goes here.]**

## 6. No warranty

6.1 The Demo is provided "as is" and "as available", without warranty of any kind, express or implied, including
merchantability, fitness for a particular purpose and non-infringement, to the extent the law allows. We do not
promise it runs on your hardware or without errors; performance figures we publish are measurements on the named
hardware, not promises.

## 7. Liability

7.1 To the extent the law allows, we are not liable for indirect, incidental, special or consequential loss, or for
lost profit, data or goodwill, arising from the Demo. Our total liability for any claim about the Demo is limited to
**[US$0 since it is free: owner and counsel confirm the cap]**. Nothing here limits liability that the law does not
allow to be limited.

## 8. Termination

8.1 This agreement ends if you break it. We may end it for everyone by publishing a notice on the Demo's download
page, for example when the Demo is withdrawn. When it ends you stop using the Demo and delete your copies. Sections
5 to 9 survive.

## 9. General

9.1 Governing law and courts: **[owner: jurisdiction; open question 2 in `docs/07-licensing.md`]**.
9.2 If a part of this agreement cannot be enforced, the rest stays in force.
9.3 This is the whole agreement about the Demo. We may publish a new version for a new Demo release; the version you
accepted applies to the copy you got.
9.4 Contact: **[owner: address for questions and takedown requests]**.

## Open points for the owner

1. **Video policy (decision 5 in `docs/showcase-plan.md`).** Proposed: allowed everywhere including monetized
   channels, no endorsement. Alternatives: (a) non-monetized only; (b) monetized only with the credit line in 3.2
   required; (c) permission by request.
2. **Redistribution of the download** (2.2): yes with the whole unmodified package (proposed), or only from our pages.
3. **Telemetry and crash upload** (4.1): none (proposed, and what the build does today), or opt-in with a first-launch
   question. The package check (`cargo xtask demo-notices --verify`) fails if an uploader is linked, so choosing
   opt-in needs a code change first.
4. **Entity, jurisdiction, contact address** (9.1, 9.4) and the liability cap (7.1).
5. **Brand.** The Demo name and whether the title screen names Regia Engine (decision 2).
6. **Match with the engine licence.** The engine is MPL-2.0 (ADR-10225); this Demo EULA covers the Demo and leaves
   the engine code under the MPL (2.4). Confirm that is wanted.
7. **Asset credits** (3.5) depend on the content pack (not delivered).
8. **Age rating and region rules** for the store, if a store is used.
