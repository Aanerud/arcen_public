# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS

"""Portable checks of the Windows Deck WiX sources.

The full contract is enforced on the built MSI by verify-msi.ps1 on Windows;
these catch the same mistakes in the sources on any OS, before a Windows
build: a WiX extension creeping in, a changed UpgradeCode, a lost legal file,
a cleanup step that would run on upgrade or impersonate the user.
"""

from __future__ import annotations

import unittest
import xml.etree.ElementTree as ET
from pathlib import Path

HERE = Path(__file__).resolve().parent.parent
NS = {"w": "http://wixtoolset.org/schemas/v4/wxs"}
LEGAL = ["LICENSE", "LICENSE-EXCEPTION.md", "NOTICE", "TRADEMARKS.md", "THIRD_PARTY_NOTICES.md"]
UPGRADE_CODES = {
    "ArcenDeck.wxs": "2DDC0F1A-DB47-42C2-AE74-EEFF30428FC4",
    "ArcenUsbComponent.wxs": "652B4B54-163A-4D12-8DCC-53B868E76B4F",
}


def load(name: str) -> ET.Element:
    return ET.parse(HERE / "wix" / name).getroot()


def file_names(root: ET.Element) -> list[str]:
    return [element.get("Source", "").rsplit("\\", 1)[-1] for element in root.iter(f"{{{NS['w']}}}File")]


class BothPackages(unittest.TestCase):
    def test_no_wix_extension_namespace(self) -> None:
        for name in UPGRADE_CODES:
            text = (HERE / "wix" / name).read_text(encoding="utf-8")
            # Only the core namespace: util, ui, firewall… would embed WiX code.
            namespaces = {part.split('"')[1] for part in text.split("xmlns")[1:] if '"' in part}
            self.assertEqual(namespaces, {NS["w"]}, name)

    def test_identity_and_upgrade(self) -> None:
        for name, code in UPGRADE_CODES.items():
            package = load(name).find("w:Package", NS)
            self.assertIsNotNone(package, name)
            assert package is not None
            self.assertEqual(package.get("UpgradeCode"), code, f"{name}: never change an UpgradeCode")
            self.assertEqual(package.get("Scope"), "perMachine", name)
            self.assertEqual(package.get("Manufacturer"), "TUTTIFRUTTI AS", name)
            upgrade = package.find("w:MajorUpgrade", NS)
            self.assertIsNotNone(upgrade, name)
            assert upgrade is not None
            self.assertEqual(upgrade.get("Schedule"), "afterInstallValidate", name)
            self.assertTrue(upgrade.get("DowngradeErrorMessage"), name)

    def test_legal_files_in_every_package(self) -> None:
        for name in UPGRADE_CODES:
            self.assertEqual(sorted(set(file_names(load(name))) & set(LEGAL)), sorted(LEGAL), name)

    def test_no_binary_payloads(self) -> None:
        for name in UPGRADE_CODES:
            root = load(name)
            self.assertEqual(list(root.iter(f"{{{NS['w']}}}Binary")), [], name)
            for action in root.iter(f"{{{NS['w']}}}CustomAction"):
                self.assertIsNone(action.get("BinaryRef"), f"{name}: {action.get('Id')}")
                self.assertIsNone(action.get("DllEntry"), f"{name}: {action.get('Id')}")
                self.assertIsNone(action.get("Script"), f"{name}: {action.get('Id')}")


class DeckPackage(unittest.TestCase):
    def setUp(self) -> None:
        self.root = load("ArcenDeck.wxs")

    def test_payload_and_no_privileged_parts(self) -> None:
        self.assertEqual(sorted(file_names(self.root)), sorted(["arcen-deck.exe", *LEGAL]))
        for tag in ("ServiceInstall", "CustomAction", "Custom"):
            self.assertEqual(list(self.root.iter(f"{{{NS['w']}}}{tag}")), [], tag)

    def test_wer_minidumps_only(self) -> None:
        keys = [
            key for key in self.root.iter(f"{{{NS['w']}}}RegistryKey")
            if key.get("Key", "").endswith(r"LocalDumps\arcen-deck.exe")
        ]
        self.assertEqual(len(keys), 1)
        values = {value.get("Name"): value for value in keys[0].findall("w:RegistryValue", NS)}
        self.assertEqual(values["DumpType"].get("Value"), "1")
        self.assertEqual(values["DumpCount"].get("Value"), "5")
        self.assertEqual(values["DumpFolder"].get("Type"), "expandable")
        self.assertEqual(values["DumpFolder"].get("Value"), r"%LOCALAPPDATA%\Arcen\Deck\CrashDumps")
        self.assertEqual(keys[0].get("Root"), "HKLM")
        self.assertEqual(keys[0].get("ForceDeleteOnUninstall"), "yes")


class UsbPackage(unittest.TestCase):
    def setUp(self) -> None:
        self.root = load("ArcenUsbComponent.wxs")

    def test_service(self) -> None:
        services = list(self.root.iter(f"{{{NS['w']}}}ServiceInstall"))
        self.assertEqual(len(services), 1)
        service = services[0]
        self.assertEqual(service.get("Name"), "ArcenUsbComponent")
        self.assertEqual(service.get("Account"), "LocalSystem")
        self.assertEqual(service.get("Start"), "auto")
        self.assertEqual(service.get("Type"), "ownProcess")
        # MsiServiceConfigFailureActions fails with error 1939 (see the .wxs).
        self.assertEqual(list(self.root.iter(f"{{{NS['w']}}}ServiceConfigFailureActions")), [])
        control = next(self.root.iter(f"{{{NS['w']}}}ServiceControl"))
        self.assertEqual(
            (control.get("Start"), control.get("Stop"), control.get("Remove")),
            ("install", "both", "uninstall"),
        )

    def actions(self) -> dict[str, ET.Element]:
        return {action.get("Id", ""): action for action in self.root.iter(f"{{{NS['w']}}}CustomAction")}

    def scheduled(self) -> dict[str, ET.Element]:
        return {custom.get("Action", ""): custom for custom in self.root.iter(f"{{{NS['w']}}}Custom")}

    def test_only_two_custom_actions_both_system_and_checked(self) -> None:
        actions = self.actions()
        self.assertEqual(sorted(actions), ["ArcenUsbServiceFailureActions", "ArcenUsbUninstallCleanup"])
        for action in actions.values():
            self.assertEqual(action.get("Execute"), "deferred")
            self.assertEqual(action.get("Impersonate"), "no")
            self.assertEqual(action.get("Return"), "check")

    def test_uninstall_cleanup_runs_our_exe_on_real_uninstall_only(self) -> None:
        cleanup = self.actions()["ArcenUsbUninstallCleanup"]
        self.assertEqual(cleanup.get("FileRef"), "ArcenUsbServiceExe")
        self.assertEqual(cleanup.get("ExeCommand"), "uninstall")
        custom = self.scheduled()["ArcenUsbUninstallCleanup"]
        self.assertEqual(custom.get("After"), "StopServices")
        # Any removal of the service executable, except by a major upgrade.
        self.assertEqual(custom.get("Condition"), "($UsbServiceExe=2) AND NOT UPGRADINGPRODUCTCODE")

    def test_failure_actions_set_by_system_sc_before_start(self) -> None:
        step = self.actions()["ArcenUsbServiceFailureActions"]
        self.assertEqual(step.get("Directory"), "System64Folder")
        self.assertTrue(step.get("ExeCommand", "").startswith('"[System64Folder]sc.exe" failure ArcenUsbComponent '))
        custom = self.scheduled()["ArcenUsbServiceFailureActions"]
        self.assertEqual(custom.get("After"), "InstallServices")
        # Not on repair: no console program runs as SYSTEM then.
        self.assertEqual(custom.get("Condition"), "NOT Installed")

    def test_install_folder_cannot_be_redirected(self) -> None:
        # A public (all upper-case) Id could be set on the msiexec command
        # line and put the LocalSystem service in a user-writable folder.
        for directory in self.root.iter(f"{{{NS['w']}}}Directory"):
            identifier = directory.get("Id", "")
            self.assertNotEqual(identifier, identifier.upper(), identifier)

    def test_empty_programdata_arcen_removed_on_uninstall(self) -> None:
        component = next(
            c for c in self.root.iter(f"{{{NS['w']}}}Component") if c.get("Id") == "UsbProgramDataCleanup"
        )
        self.assertEqual(component.get("Directory"), "ProgramDataArcenFolder")
        self.assertEqual(component.find("w:RemoveFolder", NS).get("On"), "uninstall")

    def test_payload(self) -> None:
        self.assertEqual(
            sorted(file_names(self.root)),
            sorted(["arcen-usb-service.exe", "arcen-usb-prepare.exe", *LEGAL]),
        )
        self.assertEqual(list(self.root.iter(f"{{{NS['w']}}}RegistryValue")), [])


if __name__ == "__main__":
    unittest.main()
