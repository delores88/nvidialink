"""Execute isolated NSIS rename hooks against private files and scratch registry keys."""
import json
import os
from pathlib import Path
import re
import subprocess
import unittest
import uuid

ROOT = Path(__file__).resolve().parents[1]
NSIS = Path(os.environ.get('USERPROFILE', '')) / 'AppData/Local/tauri/NSIS/makensis.exe'
UTILS = Path(os.environ.get('USERPROFILE', '')) / '.cache/sparkdev-desktop-target/release/nsis/x64/utils.nsh'


def fixture_hooks(source, key):
    # This runs the production conditions, but cannot stop a real worker, start
    # delivery, or touch the real Apps/startup keys or desktop shortcuts.
    source, count = re.subn(r"^\s*nsExec::ExecToLog 'taskkill /F /IM sparkdev-return.exe'\s*$",
                            '  DetailPrint "fixture: worker stop omitted"', source, flags=re.M)
    if count != 2:
        raise ValueError('Unexpected worker-stop hooks; review fixture isolation')
    source, count = re.subn(r'^\s*Exec .*sparkdev-return\.exe.*$',
                            '  DetailPrint "fixture: worker start omitted"', source, flags=re.M)
    if count != 1:
        raise ValueError('Unexpected worker-start hook; review fixture isolation')
    source = source.replace('Software\\DeloresAI\\DGXBridge', key + '\\LegacyProduct')
    source = source.replace('Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\DGXBridge', key + '\\LegacyApps')
    source = source.replace('Software\\Microsoft\\Windows\\CurrentVersion\\Run', key + '\\Run')
    for original, name in [('$SMPROGRAMS\\DeloresAI\\DGXBridge.lnk', 'owned-menu.lnk'),
                           ('$SMPROGRAMS\\DGXBridge.lnk', 'other-menu.lnk'),
                           ('$DESKTOP\\DGXBridge.lnk', 'owned-desktop.lnk')]:
        source = source.replace(original, '${FIXTURE_ROOT}\\' + name)
    return source


BODY = r'''
!macro Equal value expected label
  ${If} "${value}" != "${expected}"
    StrCpy $R8 "${label}"
    Goto failed
  ${EndIf}
!macroend
Section
  StrCpy $INSTDIR "${FIXTURE_ROOT}\install"
  SetOutPath $INSTDIR
  FileOpen $R9 "$INSTDIR\dgxbridge.exe" w
  FileClose $R9
  WriteRegStr HKCU "${OLD_PRODUCTKEY}" "" $INSTDIR
  StrCpy $INSTDIR "$LOCALAPPDATA\${PRODUCTNAME}"
  !insertmacro NSIS_HOOK_PREINSTALL
  !insertmacro Equal "$INSTDIR" "${FIXTURE_ROOT}\install" "manual-default-did-not-reuse-legacy-folder"
  StrCpy $INSTDIR "${FIXTURE_ROOT}\custom"
  !insertmacro NSIS_HOOK_PREINSTALL
  !insertmacro Equal "$INSTDIR" "${FIXTURE_ROOT}\custom" "explicit-custom-folder-overridden"
  StrCpy $INSTDIR "${FIXTURE_ROOT}\install"

  WriteRegStr HKCU "${OLD_PRODUCTKEY}" "" $INSTDIR
  WriteRegStr HKCU "${OLD_UNINSTKEY}" "UninstallString" '$\"$INSTDIR\uninstall.exe$\"'
  CreateShortcut "${FIXTURE_ROOT}\owned-menu.lnk" "$INSTDIR\dgxbridge.exe"
  CreateShortcut "${FIXTURE_ROOT}\other-menu.lnk" "$INSTDIR\other\dgxbridge.exe"
  CreateShortcut "${FIXTURE_ROOT}\owned-desktop.lnk" "$INSTDIR\dgxbridge.exe"
  WriteRegStr HKCU "${RUNKEY}" "DGXBridge Results" '$\"$INSTDIR\bin\sparkdev-return.exe$\" --resume-autostart --state-dir $\"$LOCALAPPDATA\SparkDevBridge$\"'
  !insertmacro NSIS_HOOK_POSTINSTALL
  ReadRegStr $6 HKCU "${OLD_UNINSTKEY}" "UninstallString"
  !insertmacro Equal "$6" "" "owned-apps-entry-remained"
  ${If} ${FileExists} "${FIXTURE_ROOT}\owned-menu.lnk"
    StrCpy $R8 "owned-menu-remained"
    Goto failed
  ${EndIf}
  ${If} ${FileExists} "${FIXTURE_ROOT}\owned-desktop.lnk"
    StrCpy $R8 "owned-desktop-remained"
    Goto failed
  ${EndIf}
  !insertmacro IsShortcutTarget "${FIXTURE_ROOT}\other-menu.lnk" "$INSTDIR\other\dgxbridge.exe"
  Pop $6
  !insertmacro Equal "$6" "1" "other-install-shortcut-deleted"
  ReadRegStr $6 HKCU "${RUNKEY}" "DGXBridge Results"
  !insertmacro Equal "$6" "" "owned-legacy-startup-remained"
  ReadRegStr $6 HKCU "${RUNKEY}" "RTXBridge Results"
  !insertmacro Equal "$6" '$\"$INSTDIR\bin\sparkdev-return.exe$\" --resume-autostart --state-dir $\"$LOCALAPPDATA\SparkDevBridge$\"' "new-startup-wrong"

  WriteRegStr HKCU "${OLD_PRODUCTKEY}" "" $INSTDIR
  WriteRegStr HKCU "${OLD_UNINSTKEY}" "UninstallString" '$\"$INSTDIR\other\uninstall.exe$\"'
  !insertmacro NSIS_HOOK_POSTINSTALL
  ReadRegStr $6 HKCU "${OLD_UNINSTKEY}" "UninstallString"
  !insertmacro Equal "$6" '$\"$INSTDIR\other\uninstall.exe$\"' "unowned-apps-entry-deleted"

  WriteRegStr HKCU "${OLD_PRODUCTKEY}" "" "$INSTDIR\other"
  WriteRegStr HKCU "${RUNKEY}" "DGXBridge Results" '$\"$INSTDIR\other\bin\sparkdev-return.exe$\" --resume-autostart --state-dir $\"$LOCALAPPDATA\SparkDevBridge$\"'
  !insertmacro NSIS_HOOK_POSTINSTALL
  ReadRegStr $6 HKCU "${OLD_PRODUCTKEY}" ""
  !insertmacro Equal "$6" "$INSTDIR\other" "other-product-registration-deleted"
  ReadRegStr $6 HKCU "${RUNKEY}" "DGXBridge Results"
  !insertmacro Equal "$6" '$\"$INSTDIR\other\bin\sparkdev-return.exe$\" --resume-autostart --state-dir $\"$LOCALAPPDATA\SparkDevBridge$\"' "other-startup-deleted-during-install"

  !insertmacro NSIS_HOOK_PREUNINSTALL
  ReadRegStr $6 HKCU "${RUNKEY}" "RTXBridge Results"
  !insertmacro Equal "$6" "" "owned-startup-remained-after-uninstall"
  ReadRegStr $6 HKCU "${RUNKEY}" "DGXBridge Results"
  !insertmacro Equal "$6" '$\"$INSTDIR\other\bin\sparkdev-return.exe$\" --resume-autostart --state-dir $\"$LOCALAPPDATA\SparkDevBridge$\"' "other-legacy-startup-deleted-during-uninstall"
  WriteRegStr HKCU "${RUNKEY}" "RTXBridge Results" '$\"$INSTDIR\other\bin\sparkdev-return.exe$\" --resume-autostart --state-dir $\"$LOCALAPPDATA\SparkDevBridge$\"'
  WriteRegStr HKCU "${RUNKEY}" "DGXBridge Results" '$\"$INSTDIR\bin\sparkdev-return.exe$\" --resume-autostart --state-dir $\"$LOCALAPPDATA\SparkDevBridge$\"'
  !insertmacro NSIS_HOOK_PREUNINSTALL
  ReadRegStr $6 HKCU "${RUNKEY}" "DGXBridge Results"
  !insertmacro Equal "$6" "" "owned-legacy-startup-remained-after-uninstall"
  ReadRegStr $6 HKCU "${RUNKEY}" "RTXBridge Results"
  !insertmacro Equal "$6" '$\"$INSTDIR\other\bin\sparkdev-return.exe$\" --resume-autostart --state-dir $\"$LOCALAPPDATA\SparkDevBridge$\"' "other-rtx-startup-deleted-during-uninstall"
  DeleteRegKey HKCU "${TEST_KEY}"
  FileOpen $R9 "${FIXTURE_ROOT}\result.json" w
  FileWrite $R9 '{"ok":true}'
  FileClose $R9
  SetErrorLevel 0
  Quit
failed:
  DeleteRegKey HKCU "${TEST_KEY}"
  FileOpen $R9 "${FIXTURE_ROOT}\result.json" w
  FileWrite $R9 '{"ok":false,"case":"$R8"}'
  FileClose $R9
  SetErrorLevel 101
  Quit
SectionEnd
'''


@unittest.skipUnless(os.name == 'nt' and NSIS.is_file() and UTILS.is_file(),
                     'Requires the cached Windows NSIS compiler and actual Tauri utils.nsh')
class InstallerHookTests(unittest.TestCase):
    def execute(self, hooks, name):
        directory = ROOT / '.local/062/hook-fixtures' / (name + '-' + uuid.uuid4().hex)
        directory.mkdir(parents=True)
        key = 'Software\\DeloresAI\\RTXBridgeHookFixture\\' + uuid.uuid4().hex
        (directory / 'hooks.nsh').write_text(fixture_hooks(hooks, key), encoding='utf-8')
        header = '\n'.join([
            'Unicode true', 'RequestExecutionLevel user', 'SilentInstall silent',
            'Name "Private RTXBridge hook regression fixture"',
            'OutFile "fixture.exe"', '!include MUI2.nsh', '!include FileFunc.nsh',
            '!include x64.nsh', '!include WordFunc.nsh', '!include "Win\\COM.nsh"',
            '!include "Win\\Propkey.nsh"', f'!include "{UTILS}"',
            f'!define FIXTURE_ROOT "{directory}"', f'!define TEST_KEY "{key}"',
            f'!define UNINSTKEY "{key}\\NewApps"', '!define PRODUCTNAME "RTXBridge"',
            '!define MAINBINARYNAME "dgxbridge"', '!define VERSION "0.6.2"',
            '!include "hooks.nsh"',
        ])
        (directory / 'fixture.nsi').write_text(header + '\n' + BODY, encoding='utf-8')
        build = subprocess.run([str(NSIS), '/V2', str(directory / 'fixture.nsi')],
                               cwd=directory, capture_output=True, timeout=30)
        self.assertEqual(build.returncode, 0, (build.stdout + build.stderr).decode('utf-8', 'replace'))
        result = subprocess.run([str(directory / 'fixture.exe'), '/S'], capture_output=True,
                                timeout=30, creationflags=subprocess.CREATE_NO_WINDOW)
        receipt = json.loads((directory / 'result.json').read_text(encoding='utf-8'))
        return result.returncode, receipt

    def test_owned_migration_and_unowned_custom_entries(self):
        code, receipt = self.execute((ROOT / 'src-tauri/installer-hooks.nsh').read_text(encoding='utf-8'), 'current')
        self.assertEqual(code, 0, receipt)
        self.assertTrue(receipt['ok'])

    def test_unconditional_shortcut_cleanup_fails_the_same_scenarios(self):
        source = (ROOT / 'src-tauri/installer-hooks.nsh').read_text(encoding='utf-8')
        old, count = re.subn(r'!macro RemoveOwnedLegacyShortcut shortcut\n.*?!macroend',
                             '!macro RemoveOwnedLegacyShortcut shortcut\n  Delete "${shortcut}"\n!macroend',
                             source, flags=re.S)
        self.assertEqual(count, 1)
        code, receipt = self.execute(old, 'unsafe-shortcut')
        self.assertEqual(code, 101, receipt)
        self.assertEqual(receipt['case'], 'other-install-shortcut-deleted')

    def test_unconditional_startup_cleanup_fails_the_same_scenarios(self):
        source = (ROOT / 'src-tauri/installer-hooks.nsh').read_text(encoding='utf-8')
        old, count = re.subn(r'!macro RemoveOwnedResultStartup name\n.*?!macroend',
                             lambda _: '!macro RemoveOwnedResultStartup name\n  DeleteRegValue HKCU "${RUNKEY}" "${name}"\n!macroend',
                             source, flags=re.S)
        self.assertEqual(count, 1)
        code, receipt = self.execute(old, 'unsafe-startup')
        self.assertEqual(code, 101, receipt)
        self.assertEqual(receipt['case'], 'other-startup-deleted-during-install')

    def test_unconditional_apps_cleanup_fails_the_same_scenarios(self):
        source = (ROOT / 'src-tauri/installer-hooks.nsh').read_text(encoding='utf-8')
        old, count = re.subn(r'    ReadRegStr \$1 HKCU "\$\{OLD_UNINSTKEY\}" "UninstallString"\n.*?    \$\{EndIf\}',
                             '    DeleteRegKey HKCU "${OLD_UNINSTKEY}"', source, flags=re.S)
        self.assertEqual(count, 1)
        code, receipt = self.execute(old, 'unsafe-apps')
        self.assertEqual(code, 101, receipt)
        self.assertEqual(receipt['case'], 'unowned-apps-entry-deleted')


if __name__ == '__main__':
    unittest.main()
