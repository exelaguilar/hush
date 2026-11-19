# winget package (Exel.Hush)

These are templates for the [winget-pkgs](https://github.com/microsoft/winget-pkgs) manifests. The installer supports `/S` (alias `--silent`) for unattended installs and must run elevated (machine scope).

To publish a release (nothing here submits anything automatically):

1. Push the `vX.Y.Z` tag and wait for the Release workflow to attach `Hush-Setup-X.Y.Z.exe` and its `.sha256`.
2. Download both into `dist\`, then run `powershell -ExecutionPolicy Bypass -File packaging\winget\update-manifests.ps1 -Version X.Y.Z`.
3. Check it: `winget validate --manifest packaging\winget\manifests\e\Exel\Hush\X.Y.Z`.
4. Fork `microsoft/winget-pkgs`, copy `manifests\e\Exel\Hush\X.Y.Z` to the same path in the fork, and open a pull request. Its validation pipeline installs the package in a VM and checks the URL and hash.
5. For later versions, repeat with the new version (the first submission needs a one-time review; updates are usually quicker).

`manifests/` is generated output and is git-ignored.
