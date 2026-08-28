# Changelog

Hand-written notes for each release. The release workflow refuses to tag a
version without a section here, and the section body becomes the GitHub
release body — which the in-app update prompt shows as patch notes. Renders
markdown in the app.

## 1.12.16

### Notes

- Supports version 2.0.5 of the game

## 1.12.15

### Language

- Additional zh-CN translations for the beta log UI page [credit to Souma-Sumire](https://github.com/Souma-Sumire)

## 1.12.14

### Features

- Added a "Compare" feature to the beta log viewer
  - Allows you to compare two logs side-by-side
  - Feature is mostly complete for the table view, but requires more work in the timeline and events view
- Added a log selector to the beta log viewer
  - This lets you navigate from log to log without having to go back to the log list
- The beta log viewer now shows Overdrive in the chart

## 1.12.13

### Notes

- Split out the modules required for Linux support from the Windows release. These were unnecessary
  for the Windows version and were one of the reasons Windows Defender would sometimes flag the application as malware

### Features

- Supplementary damage is now linked to its damage event in the event view in the beta log viewer
- The event view in the beta log viewer can be filtered by column

## 1.12.12

### Language

- Added zh-CN translation for the beta log UI page [credit to Souma-Sumire](https://github.com/Souma-Sumire)

## 1.12.11

### Features

- An opt-in **BETA** version of the new logs UI is available to try out
  - Some of the functionality is incomplete
  - The viewer is not optimized. When viewing longer fights ( ex. infinity zegalith ), it will be a bit slow

## 1.12.10

### Notes

- **Supports game patch 2.0.4.**. The patch broke a number of things so it is highly suggested you update.

### Features

- **Overmastery Predictor** can now predict rolls for several characters at once
- Perfect Summon no longer appears as a Cheat Audit violation

### Bug Fixes

- The Sigil Synthesis tool now prevents searching when your seed is out of date, to prevent the issue where the first sigil synthesis does match match the listed results

### Language

- Added missing action names and corrected the Fediel labels. [contributed by imnotsueryi-sys](https://github.com/imnotsureyi-sys)
- Clarified the zh-CN Sup column wording. [contributed by imnotsueryi-sys](https://github.com/imnotsureyi-sys)
- Updated the zh-TW translation. [contributed by o403chila](https://github.com/o403chila)

## 1.12.9

### Notes

- The Perfect Summon rule is no longer classified as a "cheat". Instead, players with Perfect Summons will be displayed in **gold** color because they are extremely lucky ✨

## 1.12.8

### Notes

- Some Cheat Audit rules have been relaxed slightly
  - The Perfect Overmasteries rule has been changed to only trigger on 3x max Stun Power from OM1. Perfect Overmastery rolls from OM3 are too commmon to be flagged. [suggested by](https://github.com/Foxschwarzengel)
  - The Perfect Summons rule now only fires when the character has **four** perfect summons which is extremely unlikely. [issue raised by](https://github.com/KakiraMikoshiJD5)

### Features

- Repeat Quest runs are now grouped in the quest list
  - The duration/IGT columns for the grouped rows display the best time for that group

### Bug Fixes

- Player level show now show correctly in the Equipment/Build tabs. [contributed by](https://github.com/imnotsureyi-sys)
- The number of rolls for Overmastery Predictor / Transmarvel Wishlist is now persistent. [reported by](https://github.com/makisevon)
- The Sup% column now properly excludes pets ( ex. Ferry's ghosts ) / summons because they cannot deal supplementary damage. [reported by](https://github.com/imnotsureyi-sys)
- Repeated quests now properly show their own in-game time instead of copying the first run of the group. reported by TJL in discord
- The Cheat Audit will now properly flag:
  - Invalid wrightstone traits
  - Immortal Shell+ sigils

## 1.12.7

### Features

- Added Import/Export functionality
  - You can now import/export log data from/to other compatible GBFR forks
  - You can now import/export user settings from Relink Logs

### Bug Fixes

- Fixed an issue where using the "Copy Screenshot to Clipboard" feature on the overlay resulted in a screenshot that had bars with no background colors

## 1.12.6

### Bug Fixes

- The "Back" button on the quest details screen now behaves as before in most cases ( return to the log list )
- Dummy actors will no longer appear in the damage meters
  - The mark applied by Essence Eustace's Tier 3 perk can be damaged and was appearing in the damage meters
- (Linux) Hook will now properly be installed
- (Linux) After this update, saved settings will persist through updates

## 1.12.5

### Notes

- **Supports game patch 2.0.3.**. The patch broke a number of things so it is highly suggested you update.

### Features

- Added a new toolbox tool: Cheat Audit
  - This reads your existing logs and builds a list of cheaters, with explanations of why
  - You can also turn on highlighting in the meter / log list / equipment / builds in the Settings section
- The UI is now more customizable
  - There are separate setting panels for cutomizing the meters and the overlay window
- The Checklist feature now supports custom groups. [suggestion from lingsamuel](https://github.com/villith/relink-logs/issues/36)

### Bug Fixes

- The connection status now also includes "Hook not found" as a status, in case your antivirus has quarantined the file. [suggestion from sgqy](https://github.com/villith/relink-logs/issues/65)
- AI teammates should now properly show up as "AI". does not apply to existing logs. [bug submitted by GGGbooy](https://github.com/villith/relink-logs/issues/64)

### Language

- Additional zh-CN translations. [credit to Souma-Sumire](https://github.com/Souma-Sumire)

## 1.12.4

### Features

- Added a new toolbox tool: Transmarvel Wishlist
  - Build a wishlist of sigils/wrightstones that you want from Transmarvel
  - This tool will tell you if any of your Transmarvel rolls will contain your wishlist items
  - This **does not** manipulate the results of Transmarvel, it is meant to be an alternative to save scumming

### Bug Fixes

- The app now ignores some common keyboard shortcuts that are not relevant (Ctrl+J, Ctrl+P, Ctrl+U). [credit to Souma-Sumire](https://github.com/Souma-Sumire)
- The connection status message should no longer incorrectly say "Hook out of date" in some cases

### Language

- Additional zh-CN translations. [credit to Souma-Sumire](https://github.com/Souma-Sumire)

## 1.12.3

### Notes

- Primal Burst damage support has been added in this build but is being **excluded** from meters by default. You can turn it on in the settings tab. There is a [poll](https://github.com/villith/relink-logs/discussions/49) available to decide on how Primal Burst should be included in the meters.
- The "Reset Overlay Layout" button was removed from the header. You can find a "Reset Windows" command by right-clicking on the tray icon instead

### Features

- The overlay and log window now show whether the parser is properly connected to the game
- Master level traits in log details are now grouped by Essence/Insight/Crux and are easier to understand at a glance

### Bug Fixes

- Fixed a memory leak that was causing issues particularly if you played with Coffinmaker Rackam.
- Sir Barrold now saves its log entries
- Player stats now appear properly ( except level )
- Stun power now shows properly ( +1 > +10, +1.2 > +12 )
- Synthesis Helper will not suggest a sigil that is equal to the sigil you are trying to craft. [reported by
lingsamuel](https://github.com/villith/relink-logs/issues/33)
- The overlay no longer forces a minimum width
- In-Game Time is now properly saved for each quest ( is not retroactive )

### Language

- The system tray menu is now localized and has a zh-CN translation [credit to Souma-Sumire](https://github.com/Souma-Sumire)
- Further zh-CN skill translations [credit to Souma-Sumire](https://github.com/Souma-Sumire)
- Djeeta abilities should appear correctly now
- Most summons should be named

## 1.12.2

### Features

- (Linux) Toolbox tools are now available ( parity with Windows client )
  - **Synthesis Helper**: choose the traits you want on a sigil and this tool will give you a list of sigils you own that, when synthesized, will result in the sigil you want
  - **Overmastery Predictor**: select the character, overmastery traits, and the minimum levels you are aiming for, and this tool will give you a list of any upcoming overmastery rolls that match your criteria

### Bug Fixes

- (Linux) Fixed a startup crash caused by newer WebKitGTK versions
- (Linux) Fixed the transparency of the overlay
- (Linux) Fixed the display of the meter rows
- Fixed an issue where if Id's first instance of damage was from his Dragon Form, he would not appear in the log details post-quest ( only applies to logs post-patch )

## 1.12.1

### Bug Fixes

- (Linux) fixed overlay text rendering behind the damage bars
- (Linux) fixed settings changes not reaching the live overlay

### Language

- zh-CN UI Language support has been greatly improved [credit to Souma-Sumire](https://github.com/Souma-Sumire)

## 1.12.0

### Features

- Releases are now code-signed via Microsoft Authenticode: Windows shows a verified publisher when installing, and antivirus false positives should decrease over time
- Perfect Guard stun values now appear in meters
- Perfect Guard (Quickening) against The World will appear as an entry in meters now
- Added a stun column for individual skills
- Added support to select/deselect columns when viewing saved logs
- Stun values that are not Perfect Guard (e.g. Eugen's sticky grenade) now show as their own row instead. If there is no label available they will appear as "Stun Effect". If you know the label, please submit it!

### Bug Fixes

- Online: other players' weapon awakening, wrightstone, and level data no longer shows stale or missing values
- Online: two players playing the same character no longer show each other's equipment
- Damage from system-generated actions (e.g. Conflux buff procs) is attributed correctly in the skill breakdown

### Language

- Beatrix and Fraux skills are now grouped by stance
- Large language updates for all languages. Most skills/sigils/traits/etc. should be labelled now.


## 1.11.1

### Bug Fixes

- Properly track Cagliostro's Pain Train and Alexandria
- Fix parent tracking of Cagliostro, Ferry, and Seofon summons

## 1.11.0

### Features

- Per-enemy damage meters
- Time range scrubber for viewing logs

### Bug Fixes

- DoTs are now properly tracked

### Language

- Added most Endless Ragnarok enemy names in all languages

## 1.10.0

### Features

- New Toolbox tool: Overmastery Predictorsearch your potential overmastery rolls for specific overmasteries
- Automatic updates can now be disabled in settings
- Overlay layout can be reset using the "Reset Overlay Layout" button

### Language

- Numerous non-english language updates for Endless Ragnarok data
