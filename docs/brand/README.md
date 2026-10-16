# Brand

ReFHIR's logo is a wordmark. "Re" is red and "FHIR" is charcoal: the standard, with the part that
keeps going set apart from it. The palette is plain on purpose, a red close to the one HL7 uses,
neutral greys and white, so ReFHIR looks at home beside the FHIR specification and does not try to
look like it.

<p>
  <img src="refhir-logo.svg" alt="ReFHIR logo" height="40">
</p>

## Files

| File | Use |
| --- | --- |
| [`refhir-logo.svg`](refhir-logo.svg) | The wordmark, for light backgrounds. The default. |
| [`refhir-logo-reversed.svg`](refhir-logo-reversed.svg) | The wordmark, for dark backgrounds. |
| [`refhir-mark.svg`](refhir-mark.svg) | The "Re" tile, red, for avatars and favicons on any background. |
| [`refhir-mark-reversed.svg`](refhir-mark-reversed.svg) | The "Re" tile in white, for places where a red tile is too loud. |
| [`refhir-icon.svg`](refhir-icon.svg), [`refhir-icon-512.png`](refhir-icon-512.png) | Square app icon, for avatars and package pages. |
| [`social-preview.png`](social-preview.png) | 1280 by 640 card. Upload it under Settings, Social preview on GitHub. |
| [`og-image.png`](og-image.png) | 1200 by 630 share image, the size link previews expect. The website uses it for Open Graph and Twitter cards. |

The website's `favicon.ico` (16, 32 and 48 pixels) and 180 pixel Apple touch icon live in `site/public/` and are
made from the icon.

The SVGs are the source of truth. The wordmark is Inter ExtraBold with tracking of -3.5%,
converted to outlines so it looks the same without the font installed. Inter is licensed under the
SIL Open Font License.

## Color

| Name | Hex | Use |
| --- | --- | --- |
| Red | `#e4281e` | "Re", the icon, and the one primary button in a view. White text on it passes AA (4.5:1). |
| Red on dark | `#ff4a3d` | The same red on dark backgrounds, where `#e4281e` is too dim to read. |
| Charcoal | `#23262b` | Text, and "FHIR" in the wordmark. |
| Grey | `#5a5f66` | Supporting text. |
| Snow | `#f5f6f7` | Panels and code blocks on white, and text on dark. |
| Line | `#d9dce0` | Borders and rules. |
| Night | `#14161a` | Dark backgrounds. |
| Link blue | `#0b62b8` | Links on white. |
| Link blue on dark | `#6fb1f2` | Links on Night. |

Pages are white. Red is the only warm color, and it marks one thing at a time: the logo, the main
button, or what has just changed or gone live (a lit row, a woken query). Links are blue so that red
keeps meaning "this is ReFHIR" or "this is the live part", and not "click here".

## Type

Inter ExtraBold (800) for the wordmark and headlines, with tight tracking. Inter Regular for body
text. IBM Plex Mono for code.

## Interface

The site is hard-edged and tactile, not soft and rounded. These rules are what make it ReFHIR:

- **Keys, not buttons.** Buttons, chips and tabs have a 2 px charcoal outline and a hard offset
  shadow with no blur. On hover they lift; on press they sink into the shadow. A chosen chip or tab
  stays pressed in. The main action is the only red key.
- **The echo.** A faint trail under the last words of the main headlines ("updates itself."),
  the same idea as the share image: the answer arriving again.
- **Rulers, not hairlines.** Sections are separated by a row of ruler ticks.
- **Numbered labels.** Each section opens with a small mono outlined label ("02 TRY IT"), and
  catalogue grids number their tiles.
- **Live is solid, static is dashed.** A panel that is kept current has a solid outline and a red top
  edge; the plain version of it is dashed and flat.

Keep new components inside these rules. Do not add gradients, glows, blurred shadows or extra accent
colors.

## Using the logo

- Leave clear space around the wordmark equal to half its height.
- Keep the wordmark at least 14 px tall (about 64 px wide) and the "Re" tile at 16 px or larger.
- Use the reversed files on Night or any dark photo, never the default ones.
- Do not recolor "Re" or "FHIR", separate them, tilt the logo, or stretch it.
- Do not use HL7's or FHIR's logos, or a flame, in ReFHIR's identity, and do not use the logo to
  suggest that HL7 endorses ReFHIR.

FHIR® is the registered trademark of HL7. ReFHIR is a separate project and is not endorsed by HL7;
the [README](../../README.md#license) carries the notice.
