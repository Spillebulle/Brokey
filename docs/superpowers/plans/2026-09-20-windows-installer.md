# Windows installer and the 0.1.5 release Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A person can download `brokey-setup-<version>-<arch>.exe`, double-click it, and end up with Brokey installed, on the Start menu, and removable from Add/Remove Programs like any other application.

**Architecture:** A hand-written WiX 5 source builds an MSI that installs `brokey.exe` and `brokey-helper.exe` side by side, because `locate_helper` finds the helper beside the running executable. The setup executable is that MSI concatenated onto Brokey's own binary with a sixteen-byte footer, a format ported from `../Muster/crates/muster-app/src/update/payload.rs` rather than reinvented. Release CI builds both assets for x64 and arm64.

**Tech Stack:** WiX 5 (`wix build`, `WixToolset.UI.wixext`, `WixToolset.Util.wixext`), Rust 2024, Python 3 with Pillow for the artwork, GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-09-19-windows-support-design.md`, the "Packaging" section (line 392) and "Self-update" (line 384).

## Global Constraints

- **The window never elevates.** A privileged operation is an entry in `brokey-helper`'s closed list. There is never an elevation call site outside `crates/brokey-core/src/transaction/elevate/`. This plan adds none: the MSI is elevated by Windows Installer, not by Brokey.
- **A source never runs anything.** `Source::plan` returns steps; the Runner runs them.
- CI sets `RUSTFLAGS: -D warnings`. An item live on one platform and dead on the other is a failed build there. The development machine cannot compile for Linux; CI is the only Linux compiler, and `crates/brokey-core/tests/transaction.rs` is `#![cfg(unix)]` at file level, so on Windows it runs zero tests and reports success. That silence is not a pass.
- Copy rules, in Rust, TypeScript, Python and installer strings alike: British spelling, sentence case, full stops in sentences, no em dashes, no emoji, says what happens. An error names what went wrong and what to do.
- Rust 2024 edition, `rust-version = "1.88"`. Let-chains are house style.
- `frontend/src/tokens.css` is never edited here. The accent hue is read from it, never retyped.
- Two assets per architecture, named exactly `brokey-<version>-<arch>.msi` and `brokey-setup-<version>-<arch>.exe`, with `<arch>` being `x64` or `arm64`, matching what Muster and Umber publish.
- **MSI rather than NSIS**, because an MSI gets a ProductCode in the uninstall keys, so Brokey can be removed silently and cleanly rather than joining the applications that cannot be. A store that leaves the mess its second feature exists to clean up would be indefensible.

## Decisions taken before this plan was written

Recorded so no task re-litigates them.

**Self-update's Windows arms are not in this plan.** The spec's "Self-update" section wants `selfupdate::install::detect` to gain Windows arms, one of them "as an MSI with a ProductCode in the uninstall keys". That needs a new allowed shape in `transaction/allow.rs`: running `msiexec` against a file Brokey downloaded, as Administrator. That is security surface and deserves its own plan and its own review. Deferring it tells no lie: after an MSI install a Windows user sees "This copy was installed in a way Brokey cannot update by itself. Update it the way it was installed.", which stays true, because updating means running the next setup executable.

**The setup window is written, not ported.** `../Muster/crates/muster-app/src/update/installwin.rs` draws with egui and `../Umber/crates/umber-app/src/splash.rs` draws with softbuffer and winit over Umber's own text and logo modules. Brokey's window is a WebView, and an installer that needs WebView2 in order to paint depends on the machine already having the component it may be there to deliver. So Task 5 draws a small Win32 window with GDI, which adds no dependency Brokey does not already have: `windows-sys` is already in `brokey-core`.

**`payload` is ported almost verbatim.** It is a byte format, it is already documented and tested in the sibling, and two statements of a byte layout is how they drift.

**GUIDs are generated fresh, never copied.** `../Muster/packaging/windows/muster.wxs` has real GUIDs in it. Copying any of them, especially `UpgradeCode`, would make installing Brokey upgrade or uninstall Muster. Every task that writes a GUID generates one.

## Verified before this plan was written

Checked against the real tree on 2026-09-20 so implementers do not re-derive it.

- `tools/make-art.py` exists and already generates the icons and banners from the palette, reading `--accent-h` from `frontend/src/tokens.css:34` (currently `300`) and converting oklch with the same arithmetic the browser uses. It requires Pillow. It does not yet write anything for the installer.
- `packaging/` holds the Linux side (`check.sh`, the desktop file, the metainfo, the polkit policy, `linux/`). There is no `packaging/windows/` yet.
- `crates/brokey/tauri.conf.json` has `"targets": ["appimage"]`, `productName` `Brokey`, identifier `io.github.spillebulle.brokey`, `frontendDist` `../../frontend/dist` and a `beforeBuildCommand` of `npm run build`. Tauri embeds the frontend into the binary at compile time, so `cargo build --release -p brokey` produces a self-contained `brokey.exe` provided `npm run build` has run first.
- `package.json` and `node_modules` are at the repository root, not in `frontend/`. The scripts are `app:dev` and `app:build`; there is no `tauri dev` script.
- `.github/workflows/release.yml` builds Linux only today: the matrix at line 27 has `ubuntu-22.04` and `ubuntu-22.04-arm`, then an Arch package job and a publish job.
- `../Muster/.github/workflows/release.yml` has a working Windows leg to copy the shape of: `windows-latest` with `x86_64-pc-windows-msvc`/`x64` and `windows-11-arm` with `aarch64-pc-windows-msvc`/`arm64` (line 33), `dotnet tool install --global wix --version 5.0.2`, `wix extension add -g WixToolset.UI.wixext/5.0.2` and `.../WixToolset.Util.wixext/5.0.2`, artwork staged into `wixassets`, `wix build ... -pdbtype none`, then `make-setup`.
- `../Muster/packaging/windows/muster.wxs` is 259 lines: `<Package>`, `<MediaTemplate EmbedCab="yes"/>`, an `INSTALLFOLDER` directory, a `StartMenu` component group with a `Shortcut` carrying `System.AppUserModel.ID`, and a `Files` component group with the executable, README, CHANGELOG and LICENSE.
- `../Muster/crates/muster-app/src/update/payload.rs` is 278 lines with `read(&[u8]) -> Option<&[u8]>`, `carried_by(&Path) -> bool` and `append(&[u8], &[u8]) -> Vec<u8>`, an eight-byte magic, a sixteen-byte footer and a `MAX_PACKAGE` bound of 512 MiB.
- `../Muster/crates/muster-app/examples/make-setup.rs` is 85 lines and takes `<executable> <package.msi> <out.exe>`.
- `crates/brokey/tests/release.rs` fails if `CHANGELOG.md` has no section for the current version or if it is not the newest.
- `../Umber/crates/umber-app/src/installart.rs` (492 lines) already generates these same two WiX bitmaps and carries the layout constraint, the measurements and the reasoning, including the one it got wrong first. `../Umber/packaging/windows/umber.wxs` (377 lines) uses `WixUI_Minimal`, the two `WixVariable` pictures, a licence RTF and a `Wix4UtilCA` exit-dialog launch. Both are closer to what Brokey needs than Muster's are, so Tasks 1 and 2 take their answers.
- `tools/banner.py` exposes `word_layer`, `ink_width`, `MARK_PER_CAP`, `GAP_PER_CAP` and `GROUNDS` (`banner.png` is `#0D0E10` on `#E6E7E9`, `banner-paper.png` is `#E4E0D9` on `#3A3836`). `banner.fit` and `banner.compose` size the brand group against the 1354x461 banner canvas alone, deliberately, so neither can be used for a 493x58 strip.

## Known gap, recorded rather than hidden

The MSI does not install or check for the WebView2 runtime. Brokey's window needs it. Windows 11 ships it, and the development machine is Windows 11 Pro 26H1, so this is invisible here and would not be on an older Windows 10 machine, where Brokey would install and then fail to start, because `lib.rs`'s `run` returns an error before the event loop begins and the `expect` beside it panics, so no window is ever shown. Detecting it means reading `HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}`, and delivering it means carrying the Evergreen bootstrapper. Both belong with whichever plan first targets Windows 10.

## File Structure

| File | Responsibility |
|---|---|
| `tools/make-art.py` (modify) | Gains the two WiX bitmaps, beside the icons it already writes |
| `packaging/windows/brokey.wxs` (create) | The MSI: two binaries, the documents, a Start menu shortcut |
| `packaging/windows/make-licence-rtf.sh` (create) | `LICENSE` to the RTF the WiX licence page needs |
| `packaging/windows/build-msi.sh` (create) | One command that builds the MSI locally and in CI |
| `crates/brokey/src/setup/payload.rs` (create) | The appended-package byte format, ported |
| `crates/brokey/src/setup/window.rs` (create) | The Win32 progress window the setup executable draws |
| `crates/brokey/src/setup/mod.rs` (create) | `--install`: unpack, show the window, run `msiexec` |
| `crates/brokey/examples/make-setup.rs` (create) | Writes `brokey-setup-*.exe` with `payload::append` |
| `crates/brokey/src/main.rs` (modify) | `--install` reaches `setup`, before the window opens |
| `.github/workflows/release.yml` (modify) | The Windows leg, two architectures, both assets |
| `CHANGELOG.md`, `Cargo.toml`, `README.md` (modify) | 0.1.5 and what it says |

---

### Task 1: The installer's two bitmaps, from the palette

**Files:**
- Modify: `tools/make-art.py`
- Create: `packaging/windows/.gitignore`

**Interfaces:**
- Produces: `packaging/windows/banner.bmp` (493x58) and `packaging/windows/dialog.bmp` (493x312), 24-bit BMP, which Task 2's `wix build` passes as `WixUIBannerBmp` and `WixUIDialogBmp`.

WiX's stock dialog set takes exactly two pictures and nothing else about its look is themeable without hand-writing the whole dialog set, so these two are the whole of "make the installer look like Brokey". They are generated rather than drawn once by hand for the reason the icons already are: an asset somebody exported once goes stale in silence, and an installer is precisely where nobody looks for the drift.

**The constraint that decides the layout, and it is not obvious.** MSI draws each dialog's title and description as **transparent text controls over the bitmap**, in the system dialog colour, which is black. There is no per-dialog way to change that: the text styles WixUI defines are shared with pages whose background is plain white, so lightening them would make those unreadable instead. **A dark bitmap under black text is not a style choice that is available.** It is an installer whose headings cannot be read.

What is available is putting the dark where the text is not. This is settled in `../Umber/crates/umber-app/src/installart.rs`, which draws these same two bitmaps and carries the measurements and the reasoning; take its answer rather than rediscovering it. Umber got the sidebar wrong once (176, which put the first page's title three pixels into a near-black ground) and the corrected figures are below.

- [ ] **Step 1: Read what is already there, in both repositories**

Read `tools/make-art.py` in full: it reads the accent hue from `frontend/src/tokens.css`, converts oklch to sRGB with `oklch_to_srgb`, and draws the mark with `mark(size, colour, scale)`. Read `tools/banner.py` for `word_layer(font_path, text, cap, ink) -> (image, baseline)`, `ink_width(font_path, text, cap) -> float`, the proportion constants `MARK_PER_CAP` and `GAP_PER_CAP`, and `GROUNDS`. Then read the module documentation and the constants of `../Umber/crates/umber-app/src/installart.rs`, down to `SIDEBAR_GAP`.

**Do not call `banner.compose` or `banner.fit` here.** `fit` sizes the brand group against the banner canvas, 1354 by 461, and deliberately nothing else, so it returns a cap of about 131 px and a mark of about 157 px. Pasted into a 58 px strip that is not a small mistake. The group is fitted to its own box here, from the same proportion constants.

- [ ] **Step 2: Write the two bitmaps**

Add to `tools/make-art.py`. Both grounds come from `banner.GROUNDS`, which is where this project's two grounds already live, so the installer cannot drift from the README banner:

```python
# WiX's stock dialog set takes exactly these two sizes and no others.
# 24-bit BMP because that is what Windows Installer's Binary table reads;
# a PNG here shows as a blank rectangle with no error anywhere.
BANNER = (493, 58)
DIALOG = (493, 312)

# Where the banner stops being light. ProgressDlg's transparent title is a
# control 330 dialog units wide starting at 20, which is 27..466 px, nearly
# the whole strip. It is left-aligned and holds "Installing Brokey", so the
# glyphs stop well short of a third of it. A judgement, not a measurement of
# the control: if a title ever did run that far the failure is text over the
# mark, which is ugly, where the sidebar's figure below would be unreadable.
BANNER_SPLIT = 300

# Where the welcome and exit field stops being dark. This is the load-bearing
# number. WelcomeEulaDlg, the whole of WixUI_Minimal's first page, draws its
# transparent title at dialog x 130, which is 173 px; ExitDialog, FatalError
# and UserExit use 135, or 180 px. 168 clears the tighter of the two by five
# pixels.
DIALOG_SIDEBAR = 168

BLOCK_MARGIN = 12      # around the brand group inside the banner's dark block
SIDEBAR_MARGIN = 24    # either side of the wordmark in the sidebar
SIDEBAR_MARK = 88      # the mark's side in the sidebar
SIDEBAR_GAP = 22       # under the mark, before the wordmark


def fitted_group(font: Path, text: str, box_w: float, box_h: float) -> float:
    """The cap height at which the mark and the wordmark fill `box`.

    `banner.fit` is not used: it measures against the banner's canvas alone,
    on purpose, so it would answer with a mark three times the height of the
    strip this is drawing.
    """
    probe = 100.0
    row_w = probe * banner.MARK_PER_CAP + probe * banner.GAP_PER_CAP + banner.ink_width(font, text, probe)
    row_h = probe * banner.MARK_PER_CAP
    return probe * min(box_w / row_w, box_h / row_h)


def installer_art(images: dict[int, "Image.Image"], colour: tuple[int, int, int]) -> None:
    """The WiX banner and dialog bitmaps.

    Light where MSI writes its black transparent titles, dark everywhere else.
    See BANNER_SPLIT and DIALOG_SIDEBAR above: this is the whole reason the
    two pictures are shaped the way they are.
    """
    out = ROOT / "packaging" / "windows"
    out.mkdir(parents=True, exist_ok=True)
    font = ROOT / "assets/fonts/Archivo.ttf"
    paper, _ = banner.GROUNDS["banner-paper.png"]
    dark, ink = banner.GROUNDS["banner.png"]

    # The banner: paper across the strip, a dark block on the right carrying
    # the brand group as a row.
    strip = Image.new("RGB", BANNER, paper)
    block_w = BANNER[0] - BANNER_SPLIT
    cap = fitted_group(font, "BROKEY", block_w - BLOCK_MARGIN * 2, BANNER[1] - BLOCK_MARGIN * 2)
    block = Image.new("RGB", (block_w, BANNER[1]), dark)
    mark_h = int(round(cap * banner.MARK_PER_CAP))
    word, baseline_in_word = banner.word_layer(font, "BROKEY", cap, ink)
    group_w = mark_h + cap * banner.GAP_PER_CAP + word.width
    left = (block_w - group_w) / 2
    middle = BANNER[1] / 2
    glyph = images[512].resize((mark_h, mark_h), Image.LANCZOS)
    block.paste(glyph, (int(round(left)), int(round(middle - mark_h / 2))), glyph)
    block.paste(word, (int(round(left + mark_h + cap * banner.GAP_PER_CAP)),
                       int(round(middle + cap / 2 - baseline_in_word))), word)
    strip.paste(block, (BANNER_SPLIT, 0))
    strip.save(out / "banner.bmp")

    # The field: paper, with a dark sidebar down the left. The row does not
    # fit a 168 px column at any size worth reading, so the sidebar stacks the
    # mark over the wordmark. It is the only second arrangement of the brand
    # group in Brokey and it is stated here, once.
    field = Image.new("RGB", DIALOG, paper)
    field.paste(Image.new("RGB", (DIALOG_SIDEBAR, DIALOG[1]), dark), (0, 0))
    column = DIALOG_SIDEBAR
    usable = column - SIDEBAR_MARGIN * 2
    probe = 100.0
    cap = probe * usable / banner.ink_width(font, "BROKEY", probe)
    word, baseline_in_word = banner.word_layer(font, "BROKEY", cap, ink)
    group_h = SIDEBAR_MARK + SIDEBAR_GAP + cap
    top = (DIALOG[1] - group_h) / 2
    glyph = images[512].resize((SIDEBAR_MARK, SIDEBAR_MARK), Image.LANCZOS)
    field.paste(glyph, (int(round((column - SIDEBAR_MARK) / 2)), int(round(top))), glyph)
    field.paste(word, (int(round((column - word.width) / 2)),
                       int(round(top + SIDEBAR_MARK + SIDEBAR_GAP + cap - baseline_in_word))), word)
    field.save(out / "dialog.bmp")
    print(f"wrote packaging/windows/banner.bmp and dialog.bmp, split {BANNER_SPLIT} and {DIALOG_SIDEBAR}")
```

Call it from `main()` after the icons are written, passing `images` and `colour`.

- [ ] **Step 3: Make the script refuse to write an unreadable installer**

This is the step that matters, and it is a check on the bytes that were actually written rather than on the values that were meant to be. Umber's comment on the equivalent test earns its place: the sidebar was 176 first, and moving the constant to 168 without redrawing would have left the committed bitmap unchanged and every check green.

Add to `tools/make-art.py`, called at the end of `installer_art` and reading the two files back from disk:

```python
def readable(path: Path, size: tuple[int, int], light: tuple[int, int, int, int]) -> None:
    """Refuse to leave a bitmap MSI would write black text onto illegibly.

    `light` is the box, in pixels, that must stay pale: MSI draws the dialog
    titles there and the colour is not ours to change.
    """
    img = Image.open(path)
    if img.size != size or img.mode != "RGB":
        sys.exit(f"{path.name} is {img.size} {img.mode}, and WiX takes {size} 24-bit")
    x0, y0, x1, y1 = light
    px = img.load()
    darkest = min(
        (0.2126 * px[x, y][0] + 0.7152 * px[x, y][1] + 0.0722 * px[x, y][2]) / 255
        for y in range(y0, y1) for x in range(x0, x1)
    )
    if darkest <= 0.6:
        sys.exit(f"{path.name} is too dark at {darkest:.2f} where MSI writes its titles in black")
```

Call it as `readable(out / "banner.bmp", BANNER, (0, 0, BANNER_SPLIT, BANNER[1]))` and `readable(out / "dialog.bmp", DIALOG, (DIALOG_SIDEBAR, 0, DIALOG[0], DIALOG[1]))`.

- [ ] **Step 4: Run it and look at the result**

Run: `python3 tools/make-art.py`

Expected: the icon lines it always printed, then the new line, and no exit. A `SystemExit` naming a luminance is the check doing its job.

Then open both files and look at them. The banner should read as a pale strip with a dark tile on the right carrying the mark and "BROKEY"; the field should read as a dark column on the left with the mark above the word, and paper to the right of it. An implementer who has not looked at these two pictures has not finished this task.

- [ ] **Step 5: Decide whether the bitmaps are committed**

- [ ] **Step 4: Decide whether the bitmaps are committed**

They are generated, and the icons the same script writes **are** committed, so these are too: CI must not need Pillow. Create `packaging/windows/.gitignore` holding only the WiX build leftovers:

```gitignore
*.wixpdb
*.msi
```

- [ ] **Step 6: Commit**

```bash
git add tools/make-art.py packaging/windows/banner.bmp packaging/windows/dialog.bmp packaging/windows/.gitignore
git commit -m "Art: the installer's two bitmaps, from the palette"
```

---

### Task 2: The MSI

**Files:**
- Create: `packaging/windows/brokey.wxs`
- Create: `packaging/windows/make-licence-rtf.sh`
- Create: `packaging/windows/build-msi.sh`

**Interfaces:**
- Consumes: `packaging/windows/banner.bmp` and `dialog.bmp` from Task 1.
- Produces: `dist/brokey-<version>-<arch>.msi`, installing `brokey.exe` and `brokey-helper.exe` into one directory. Task 4 concatenates this file; Task 6's CI builds it.

**This is the task that makes Brokey installable, and it is independently testable on the development machine, which is Windows.**

- [ ] **Step 1: Read the siblings, Umber first**

Read `../Umber/packaging/windows/umber.wxs` in full, then `../Muster/packaging/windows/muster.wxs`. Umber's is the better reference: it is newer, it carries the reasoning for every decision in its comments, and several of those comments record a build that failed first. Copy its structure and the register of its comments. **Do not copy a single GUID from either.**

Four traps it records, each of which cost somebody a build:

1. **An XML comment may not contain two hyphens in a row.** A long option written into a comment is a parse error, and it is what broke the first build of Umber's file. Write `the -pdbtype option` or reword.
2. **The `Icon` Id is a file name, not a label, and the extension is load-bearing.** Windows Installer streams each Icon row out to a real file named with the Id, and the shell identifies it by that name alone. Use `brokey.ico`, not `BrokeyIcon`, and point `ARPPRODUCTICON` at it.
3. **The Start menu shortcut names no icon and is not advertised.** The Icon table requires a shortcut's icon to be in EXE binary format with a matching extension, so no `.ico` row can serve it. A plain `.lnk` takes its icon from the executable it points at, which for Brokey is the `RT_GROUP_ICON` that `tauri-build` compiles in from `crates/brokey/icons/icon.ico`.
4. **A non-advertised shortcut needs an HKCU registry value as its component's keypath**, or ICE43 fails the build, and validation runs as part of `wix build`. Do not tidy that root to HKMU or HKLM: ICE57 refuses per-user and per-machine data in one component and reports it at error severity. Umber's comment on this is worth reading before changing it.

- [ ] **Step 2: Generate fresh GUIDs**

```sh
python3 -c "import uuid; [print(uuid.uuid4()) for _ in range(5)]"
```

Keep all five. They are the `UpgradeCode` and one per `Component`. Write them into the file as literals: a GUID that changes between builds breaks upgrades.

- [ ] **Step 3: Write `packaging/windows/brokey.wxs`**

The parts that differ from Muster, and which the rest of this plan depends on:

```xml
<!--
  Two executables, not one. `transaction::runner::locate_helper` on Windows
  looks for `brokey-helper.exe` beside the running executable, so the helper
  is not an optional extra: without it in this directory every plan fails at
  the point of elevating, which is the last moment anyone would look.
-->
<ComponentGroup Id="Files" Directory="INSTALLFOLDER">
  <Component Id="Executable" Guid="PUT-FRESH-GUID-1-HERE">
    <File Id="BrokeyExe" Source="$(var.BinDir)\brokey.exe" KeyPath="yes" />
  </Component>
  <Component Id="Helper" Guid="PUT-FRESH-GUID-2-HERE">
    <File Id="BrokeyHelperExe" Source="$(var.BinDir)\brokey-helper.exe" KeyPath="yes" />
  </Component>
  <Component Id="Documents" Guid="PUT-FRESH-GUID-3-HERE">
    <File Source="$(var.DocDir)\README.md" />
    <File Source="$(var.DocDir)\CHANGELOG.md" />
    <File Source="$(var.DocDir)\LICENSE" Name="LICENSE.txt" />
  </Component>
</ComponentGroup>
```

The dialog set and the two pictures, which is the rest of why `WixToolset.UI.wixext` is a dependency:

```xml
<!--
  WixUI_Minimal: a licence page, a progress page, and done. Brokey has one
  feature and one directory worth choosing, so a feature tree and a directory
  browser would be three extra pages offering a choice nobody has. It also
  sets ARPNOMODIFY, so the Add/Remove Programs entry offers Remove alone
  rather than a Change that leads nowhere.
-->
<ui:WixUI Id="WixUI_Minimal" />
<WixVariable Id="WixUILicenseRtf" Value="$(var.AssetDir)\licence.rtf" />
<WixVariable Id="WixUIBannerBmp" Value="$(var.AssetDir)\banner.bmp" />
<WixVariable Id="WixUIDialogBmp" Value="$(var.AssetDir)\dialog.bmp" />
```

The exit page offers to start Brokey, which is what `WixToolset.Util.wixext` is for, and it is the one place in this file where Brokey's central invariant is at stake:

```xml
<!--
  "Start Brokey" on the last page, ticked.

  `Impersonate="yes"` is the part that matters, and it matters more here than
  it does in any sibling. Brokey installs per machine, so the installer is
  elevated. Without this attribute the custom action would start Brokey as
  the elevated account, and Brokey's first invariant is that its window never
  runs as Administrator: the window builds plans and `brokey-helper` runs the
  privileged steps, one prompt per stretch. A window started elevated here
  would quietly have the privilege the whole design exists to withhold, and
  would write its settings into the wrong profile as well.

  Deferred to the Finish button and conditioned on `NOT Installed`, so a
  repair does not launch the application behind the user's back.
-->
<Property Id="WIXUI_EXITDIALOGOPTIONALCHECKBOXTEXT" Value="Start Brokey" />
<Property Id="WIXUI_EXITDIALOGOPTIONALCHECKBOX" Value="1" />
<Property Id="WixShellExecTarget" Value="[#BrokeyExe]" />
<CustomAction Id="StartBrokey"
              BinaryRef="Wix4UtilCA_$(sys.BUILDARCHSHORT)"
              DllEntry="WixShellExec"
              Impersonate="yes" />
<UI>
  <Publish Dialog="ExitDialog"
           Control="Finish"
           Event="DoAction"
           Value="StartBrokey"
           Condition="WIXUI_EXITDIALOGOPTIONALCHECKBOX = 1 and NOT Installed" />
</UI>
```

`[#BrokeyExe]` is the `File` Id from the component group above, which is why that element names its Id explicitly.

Install per machine (`Scope="perMachine"`, `Compressed="yes"`) into `ProgramFiles6432Folder`, which resolves to the native Program Files on both architectures and is the one Umber uses for exactly this reason. The uninstall entry then lands in HKLM and Brokey appears in Add/Remove Programs for every user.

The Start menu shortcut, with the keypath ICE43 requires:

```xml
<StandardDirectory Id="ProgramMenuFolder" />

<ComponentGroup Id="StartMenu" Directory="ProgramMenuFolder">
  <Component Id="StartMenuShortcut" Guid="PUT-FRESH-GUID-4-HERE">
    <Shortcut Id="BrokeyShortcut"
              Name="Brokey"
              Description="Install and update software"
              Target="[#BrokeyExe]"
              WorkingDirectory="INSTALLFOLDER"
              Advertise="no">
      <ShortcutProperty Key="System.AppUserModel.ID" Value="io.github.spillebulle.brokey" />
    </Shortcut>
    <RegistryValue Root="HKCU"
                   Key="Software\Brokey contributors\Brokey"
                   Name="StartMenuShortcut"
                   Type="integer"
                   Value="1"
                   KeyPath="yes" />
  </Component>
</ComponentGroup>
```

The `System.AppUserModel.ID` is `io.github.spillebulle.brokey`, the identifier already in `crates/brokey/tauri.conf.json:5`.

Also include a `MajorUpgrade` with `AllowSameVersionUpgrades="yes"` and a `DowngradeErrorMessage`, so 0.1.6 replaces 0.1.5 rather than installing beside it and an older installer refuses rather than quietly downgrading. Brokey has no document types, so Umber's file-association section has no counterpart here: leave it out.

Take `Name`, `Manufacturer`, `Version` and `UpgradeCode` from `$(var.Version)` and literals. `MediaTemplate EmbedCab="yes"` keeps it one file.

- [ ] **Step 4: Write the licence converter**

`packaging/windows/make-licence-rtf.sh`, ported from `../Muster/packaging/windows/make-licence-rtf.sh`. WiX's licence page reads RTF and shows a plain text file as one unwrapped line.

- [ ] **Step 5: Write `packaging/windows/build-msi.sh`**

One script so a person and CI run the same thing:

```sh
#!/bin/sh
# Builds dist/brokey-<version>-<arch>.msi from an already-built binary pair.
#   sh packaging/windows/build-msi.sh <version> <arch> <bindir>
# Needs: wix 5, WixToolset.UI.wixext, WixToolset.Util.wixext.
set -eu
version="$1"; arch="$2"; bindir="$3"
mkdir -p wixassets dist
cp assets/icons/brokey.ico wixassets/brokey.ico
cp packaging/windows/banner.bmp packaging/windows/dialog.bmp wixassets/
sh packaging/windows/make-licence-rtf.sh LICENSE wixassets/licence.rtf
# `-pdbtype none`: WiX otherwise drops a .wixpdb beside the installer, which
# is not something to publish and not something to explain.
wix build packaging/windows/brokey.wxs \
  -arch "$arch" \
  -ext WixToolset.UI.wixext \
  -ext WixToolset.Util.wixext \
  -pdbtype none \
  -d Version="$version" \
  -d BinDir="$bindir" \
  -d DocDir="." \
  -d AssetDir="wixassets" \
  -o "dist/brokey-${version}-${arch}.msi"
```

- [ ] **Step 6: Build it here**

```sh
dotnet tool install --global wix --version 5.0.2
wix extension add -g WixToolset.UI.wixext/5.0.2
wix extension add -g WixToolset.Util.wixext/5.0.2
npm run build
cargo build --release -p brokey -p brokey-helper
sh packaging/windows/build-msi.sh 0.1.4 x64 target/release
```

Expected: `dist/brokey-0.1.4-x64.msi` exists. A WiX error naming a missing `-d` variable means the `.wxs` uses a name `build-msi.sh` does not pass.

- [ ] **Step 7: Install it, use it, remove it**

This is the point of the task, and a report that skips it is not a report.

```powershell
msiexec /i dist\brokey-0.1.4-x64.msi /qn /l*v install.log
```

Then check, and record each answer:
1. `"C:\Program Files\Brokey\brokey.exe"` and `brokey-helper.exe` are both there.
2. `brokey.exe --version` prints the version.
3. Brokey appears in Add/Remove Programs with a publisher and a version.
4. The Start menu has a Brokey entry that opens the window.
5. Opening the window and installing a small winget package works from the installed copy, which is the whole chain: installed `brokey.exe` finds the installed `brokey-helper.exe` and elevates it.
6. `msiexec /x dist\brokey-0.1.4-x64.msi /qn` removes it and leaves no files in `C:\Program Files\Brokey`.

**Never write a transcript you did not see.** If something fails, that is the finding and it is worth more than a green build.

- [ ] **Step 8: Commit**

```bash
git add packaging/windows/brokey.wxs packaging/windows/make-licence-rtf.sh packaging/windows/build-msi.sh
git commit -m "Packaging: an MSI that installs Brokey and its helper"
```

---

### Task 3: The appended package format

**Files:**
- Create: `crates/brokey/src/setup/mod.rs`
- Create: `crates/brokey/src/setup/payload.rs`
- Modify: `crates/brokey/src/lib.rs`

**Interfaces:**
- Produces: `setup::payload::read(&[u8]) -> Option<&[u8]>`, `setup::payload::carried_by(&Path) -> bool`, `setup::payload::append(&[u8], &[u8]) -> Vec<u8>`. Task 4's example calls `append`; Task 5's `--install` calls `read`.

- [ ] **Step 1: Port the module**

Copy `../Muster/crates/muster-app/src/update/payload.rs` to `crates/brokey/src/setup/payload.rs` and change only what must change: the magic bytes, the doc comment's file names, and `MAX_PACKAGE`'s justifying sentence if Brokey's MSI is a different size. Keep every other comment, including the one saying the magic is not a signature and must not be described as one. Keep its tests.

The magic is eight bytes so a file ending in a plausible length is not mistaken for a payload. Choose Brokey's own rather than inheriting Muster's `UMBRPKG\0`: a file carrying one project's payload must not read as the other's.

- [ ] **Step 2: Create the module and declare it**

`crates/brokey/src/setup/mod.rs`:

```rust
//! The setup executable: Brokey's own binary with an MSI on the end of it.
//!
//! Run normally this is just Brokey. Run with `--install` it lifts the MSI
//! back out of its own file and installs it through a window of Brokey's own,
//! so somebody installing Brokey for the first time sees Brokey rather than
//! Windows Installer.

pub mod payload;
```

Add `pub mod setup;` to `crates/brokey/src/lib.rs`.

- [ ] **Step 3: Run the ported tests**

Run: `cargo test -p brokey setup::payload`

Expected: every test that passed in the sibling passes here. A round trip, a plain executable answering `None`, a truncated footer answering `None`, and a length beyond `MAX_PACKAGE` answering `None` rather than panicking.

- [ ] **Step 4: Add the test the port needs that the sibling does not**

```rust
/// Brokey's magic is not Muster's. A setup executable of one project must
/// not read as the other's, because both are installed by the same person
/// from the same folder and both end in `-setup-<version>-<arch>.exe`.
#[test]
fn the_magic_is_brokeys_own() {
    assert_ne!(MAGIC, b"UMBRPKG\0");
    assert_eq!(MAGIC.len(), 8, "eight bytes, so a plausible length is not mistaken for one");
}
```

- [ ] **Step 5: Check both platforms**

Run: `cargo test --workspace`, then `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets`, then `cargo fmt --all --check`.

The module is pure byte handling and must compile on Linux too, where it is dead but harmless. If `-D warnings` objects to it being unused on Linux, gate the *module* rather than weakening the test.

- [ ] **Step 6: Commit**

```bash
git add crates/brokey/src/setup/ crates/brokey/src/lib.rs
git commit -m "Setup: the package an executable can carry on its end"
```

---

### Task 4: `make-setup`

**Files:**
- Create: `crates/brokey/examples/make-setup.rs`

**Interfaces:**
- Consumes: `brokey_lib::setup::payload::append` from Task 3.
- Produces: `brokey-setup-<version>-<arch>.exe`, which Task 6's CI publishes.

A Rust example rather than a shell script, unlike the rest of `packaging/`, for one reason: it calls the same `append` the running binary reads with, so the writer and the reader cannot drift.

- [ ] **Step 1: Port it**

From `../Muster/crates/muster-app/examples/make-setup.rs`. It takes `<executable> <package.msi> <out.exe>`, reads both, calls `append`, writes the result, and then reads its own output back through `payload::read` to check its work before exiting. Keep that check: it is the reason the sibling's comment says the format is checked twice.

- [ ] **Step 2: Build a setup executable here**

```sh
cargo run --release -p brokey --example make-setup -- \
  target/release/brokey.exe \
  dist/brokey-0.1.4-x64.msi \
  dist/brokey-setup-0.1.4-x64.exe
```

Expected: it prints the sizes and exits 0.

- [ ] **Step 3: Check the result is still a working executable**

```sh
./dist/brokey-setup-0.1.4-x64.exe --version
```

Expected: the version prints. Windows loads a PE by its headers rather than by the file's length, so the bytes after the last section are ignored and the setup executable runs exactly as `brokey.exe` does. If this fails, the payload was written into the file rather than after it.

- [ ] **Step 4: Commit**

```bash
git add crates/brokey/examples/make-setup.rs
git commit -m "Setup: make-setup writes the executable that carries the MSI"
```

---

### Task 5: `--install`, and the window it draws

**Files:**
- Create: `crates/brokey/src/setup/window.rs`
- Modify: `crates/brokey/src/setup/mod.rs`
- Modify: `crates/brokey/src/main.rs`

**Interfaces:**
- Consumes: `payload::read` from Task 3, `is_text_mode` in `crates/brokey/src/main.rs`.
- Produces: `setup::install() -> i32`, reached by `--install` before any Tauri code runs.

**This is the largest task and the only one with no sibling to copy.** Muster's installer window is egui and Umber's splash is softbuffer and winit over Umber's own text and logo modules. Brokey's window is a WebView, and an installer that needs WebView2 in order to paint depends on the machine having the component it may be there to deliver. So this draws a small Win32 window with GDI.

- [ ] **Step 1: Decide where `--install` is handled, and write the failing test**

`--install` begins with a dash, so `is_text_mode` in `crates/brokey/src/main.rs` currently sends it to the window. It must not. Add to that file's `mod tests`:

```rust
/// `--install` is the setup executable's own flag and must never open the
/// window: the window is what it is there to install.
#[test]
fn the_install_flag_is_not_the_window() {
    assert!(is_text_mode(&args(&["--install"])));
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p brokey --bin brokey the_install_flag_is_not_the_window`

Expected: FAIL, because `--install` starts with a dash and is not in `TEXT_FLAGS`.

- [ ] **Step 3: Route it**

Add `"--install"` to `TEXT_FLAGS` in `crates/brokey/src/main.rs`, and in `brokey_lib::cli::run` add an arm that calls `brokey_lib::setup::install()` on Windows and, on Linux, prints that the setup executable is a Windows thing and returns 2. Both sentences follow the copy rules.

- [ ] **Step 4: Write the window**

`crates/brokey/src/setup/window.rs`, `#[cfg(windows)]` throughout. A single `CreateWindowExW` window with a `WNDPROC`, painted in `WM_PAINT` with GDI:

- The ground and the accent from the same values `tools/make-art.py` computes, written here as literals with a comment naming `frontend/src/tokens.css:34` as where they come from and why they are not read at runtime.
- A hairline, the title "Install Brokey <version>", one primary button, and an empty progress track while `msiexec` runs, exactly as the spec describes.
- Progress is honest: `msiexec` reports nothing usable, so the track stays empty with a sentence beside it rather than showing a percentage nobody measured. This is the same invariant the application follows.

Every `unsafe` block carries a `// SAFETY:` comment saying why it holds, matching `crates/brokey-core/src/transaction/elevate/windows.rs`.

- [ ] **Step 5: Write the install path**

In `crates/brokey/src/setup/mod.rs`:

```rust
/// `--install`: lift the MSI out of this file, put it somewhere msiexec can
/// read, and run it while the window says what is happening.
///
/// The version comes from this file's own stem, so
/// `brokey-setup-0.1.5-x64.exe` says "Install Brokey 0.1.5" at the top of
/// the window and a renamed copy says whatever it was renamed to. That is
/// the caller's business and it is documented rather than prevented.
#[cfg(windows)]
pub fn install() -> i32
```

It reads its own executable, calls `payload::read`, writes the MSI to a temporary directory, runs `msiexec /i <file> /qn` and reports the outcome in the window. A missing payload is the ordinary case for a plain `brokey.exe` and gets a sentence saying this copy carries no installer, not a panic.

- [ ] **Step 6: Test what can be tested without installing**

A test that `install()` on a binary with no payload reports the "carries no installer" answer rather than running `msiexec`. **No test may run `msiexec`, elevate, or install anything.**

- [ ] **Step 7: Run the real thing**

```sh
./dist/brokey-setup-0.1.4-x64.exe --install
```

Expected: Brokey's own window appears, UAC asks once, the track sits empty with its sentence, and Brokey ends up installed. Record what appeared, what the window said, and whether the result matches Task 2's checklist. Then remove it again through Add/Remove Programs.

- [ ] **Step 8: Commit**

```bash
git add crates/brokey/src/setup/ crates/brokey/src/main.rs
git commit -m "Setup: --install unpacks the MSI behind Brokey's own window"
```

---

### Task 6: The Windows leg of the release, and 0.1.5

**Files:**
- Modify: `.github/workflows/release.yml`
- Modify: `Cargo.toml`, `crates/brokey/Cargo.toml`, `crates/brokey-core/Cargo.toml`, `crates/brokey-helper/Cargo.toml`, `package.json`
- Modify: `CHANGELOG.md`, `README.md`

**Interfaces:**
- Consumes: `packaging/windows/build-msi.sh` from Task 2 and `make-setup` from Task 4.

- [ ] **Step 1: Add the matrix rows**

In `.github/workflows/release.yml`, beside the two Linux rows at line 27:

```yaml
- { os: windows-latest,  target: x86_64-pc-windows-msvc,  arch: x64   }
- { os: windows-11-arm,  target: aarch64-pc-windows-msvc, arch: arm64 }
```

Every existing step that is Linux-only gains an `if: runner.os == 'Linux'`. Read each one and decide; a step that silently runs on Windows and does nothing is worse than one that is skipped on purpose.

- [ ] **Step 2: Add the Windows steps**

Copy the shape from `../Muster/.github/workflows/release.yml` lines 100 to 165, with `if: runner.os == 'Windows'`: install `wix --version 5.0.2` and both extensions, `npm ci && npm run build`, `cargo build --release -p brokey -p brokey-helper`, `sh packaging/windows/build-msi.sh "$VERSION" "${{ matrix.arch }}" target/release`, then `make-setup`. Upload both files.

Pin the WiX version and both extension versions exactly, as Muster does. An installer that builds differently next month is not a release process.

- [ ] **Step 3: Bump the version, in all three places the guard checks**

`crates/brokey/tests/release.rs` checks **three** places, and its own module documentation names them: the Cargo workspace, `crates/brokey/tauri.conf.json` (what the AppImage and the window's About say) and `package.json` (what the Tauri build reads). Miss `tauri.conf.json` and `the_tauri_config_carries_this_version` fails; miss `package.json` and `the_package_json_carries_this_version` fails.

0.1.4 to 0.1.5 in the workspace `Cargo.toml`, `crates/brokey/tauri.conf.json`, and `package.json`. The member crates use `version.workspace = true`, so they need no edit; check that rather than assuming it. Then `cargo build --workspace` so `Cargo.lock` follows, and `npm install --package-lock-only` if `package-lock.json` records the version.

- [ ] **Step 4: Write the CHANGELOG entry**

`crates/brokey/tests/release.rs` fails if `CHANGELOG.md` has no section for the current version or if it is not the newest. Write what 0.1.5 actually is: Windows installs, updates and removes software through winget and Add/Remove Programs; there is an installer; the window never elevates. Name what is still missing, in the register the file already uses.

- [ ] **Step 5: Correct the README, including the table no test guards**

Three separate edits, and the second is the one that gets forgotten:

1. The Windows bullet in "What is not there yet" (`README.md:105`) says "There is no installer yet, so Windows is built from source." That stops being true with this plan. Say how to install instead, and leave the rest of the bullet's honesty alone: the missing icons and descriptions, the duplicate rows, and Chocolatey, Scoop and the Store are all still true.

2. **The download table at `README.md:23-32` hardcodes `0.1.4` and eight `v0.1.4` release URLs, and nothing tests it.** `crates/brokey/tests/release.rs` says in its own documentation that Muster's README download-link guards were left out "because Brokey's README does not carry a download table yet" — which was true when that file was written and is not true now. Bump the heading and every URL to 0.1.5, and add the two Windows rows:

| Windows 11, Windows 10 | [`setup.exe`](https://github.com/Spillebulle/Brokey/releases/download/v0.1.5/brokey-setup-0.1.5-x64.exe) | [`setup.exe`](https://github.com/Spillebulle/Brokey/releases/download/v0.1.5/brokey-setup-0.1.5-arm64.exe) |

Mention that the `.msi` is published beside it for anyone deploying Brokey across a fleet.

3. The banner sentence at `README.md:9` says "One store for every way a **Linux** machine gets software". Brokey now installs software on Windows. Correct it.

**Ruling, carried in this plan rather than left to the implementer:** do not add a README download-link guard to `release.rs` in this task. It is the right test and Muster has one to transcribe, but it belongs with the release plumbing rather than bolted onto a version bump, and adding it here would mean writing a test and a table in the same commit with no independent check on either. Record it instead as the first item of whatever plan next touches `release.yml`.

- [ ] **Step 6: Check everything**

Run: `cargo test --workspace`, `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets`, `cargo fmt --all --check`, `npm run lint`, `npm run check:design`, `sh packaging/check.sh`.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "Release: 0.1.5, with a Windows installer"
```

---

## Self-review

**Spec coverage.** The "Packaging" section's two assets are Tasks 2 and 4, its WiX 5 and `WixToolset.UI.wixext` are Task 2, its generated artwork is Task 1, its `update::payload` port is Task 3, its Win32 setup window is Task 5, and its "both assets are published" is Task 6. The "Self-update" section is deliberately not covered, with the reasoning recorded above. Its winget, Scoop and Chocolatey manifests are out of scope by the plan's own framing and belong with the sources that serve them.

**Gaps, recorded rather than hidden.**

1. No code signing. An unsigned setup executable raises SmartScreen, which is the first thing a person sees and the last thing this plan addresses. The spec does not discuss it. It needs a certificate, which is a purchase, not a task.
2. WebView2 is not installed or detected. See the known gap above.
3. arm64 is built and never run. There is no ARM64 Windows machine in this project, so the arm64 MSI and setup executable are CI artefacts nobody has opened. The same was true of the Linux ARM packages before them.
4. Task 5 has no sibling to copy and is the one place this plan asks for real design work. If it overruns, Tasks 1, 2, 3, 4 and 6 still deliver a working MSI, which is installable and testable on its own; the setup executable is how it is distributed, not whether it works.

**Type consistency.** `payload::read`, `payload::carried_by` and `payload::append` are named in Task 3 and used under those names in Tasks 4 and 5. `setup::install()` is defined in Task 5 and routed in Task 5. `is_text_mode` and `TEXT_FLAGS` are the names already in `crates/brokey/src/main.rs`. `build-msi.sh`'s `-d` variables (`Version`, `BinDir`, `DocDir`, `AssetDir`) are the names Task 2's `.wxs` must use.

**What every implementer must do.** The development machine is Windows and cannot compile for Linux, so every task that touches shared code carries a **Verified by running:** line and a **Reasoned, not run:** line, and CI is what settles the second. Tasks 2 and 5 end in a real install on a real machine: record what happened, and never write a transcript you did not see.
