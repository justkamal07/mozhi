# Signing Mozhi releases

Unsigned builds work, but macOS and Windows warn people before opening them. The release
workflow (`.github/workflows/release.yml`) signs automatically as soon as the secrets below
exist in the GitHub repository. Nothing else needs to change: add the secrets, push a new tag
(for example `v0.3.2`) and that release comes out signed.

Add each secret with the GitHub CLI (it prompts for the value, so nothing lands in your shell
history), or in the repository under Settings → Secrets and variables → Actions:

```bash
gh secret set APPLE_CERTIFICATE --repo justkamal07/mozhi
```

## macOS: Developer ID + notarisation

This removes the right-click → Open step. It also keeps the Accessibility permission across
updates, because macOS ties that permission to a stable signing identity.

1. Join the **Apple Developer Program** as an individual (developer.apple.com/programs, US$99 a
   year at the time of writing).
2. Create a **Developer ID Application** certificate: Xcode → Settings → Accounts → your team →
   Manage Certificates → + → Developer ID Application.
3. In Keychain Access, export that certificate *with its private key* as a `.p12` file and choose a
   password.
4. Find the identity name: `security find-identity -v -p codesigning`. It looks like
   `Developer ID Application: Your Name (ABCDE12345)`; the part in brackets is your Team ID.
5. Create an **app-specific password** for notarisation at account.apple.com → Sign-In and
   Security → App-Specific Passwords.

| Secret | Value |
| --- | --- |
| `APPLE_CERTIFICATE` | the `.p12` file as base64: `base64 -i DeveloperID.p12 \| pbcopy`, then paste |
| `APPLE_CERTIFICATE_PASSWORD` | the password you set when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | `Developer ID Application: Your Name (ABCDE12345)` |
| `APPLE_ID` | your Apple Account email |
| `APPLE_PASSWORD` | the app-specific password (not your Apple Account password) |
| `APPLE_TEAM_ID` | the 10-character Team ID |

The first three sign the app; the last three notarise it. Signing without notarisation still
shows a warning on recent macOS versions, so add all six.

## Windows: Azure Trusted Signing

This replaces the SmartScreen "unrecognised app" screen with your verified name. Microsoft's
Azure Trusted Signing costs about US$10 a month at the time of writing, and Microsoft verifies
your identity first. Check its current eligibility rules for individuals in your country before
you sign up.

1. In the Azure portal, create a **Trusted Signing account**, complete identity validation, and
   create a **certificate profile** (Public Trust).
2. Create an **app registration** (service principal) with a client secret, and give it the
   *Trusted Signing Certificate Profile Signer* role on the account.

| Secret | Value |
| --- | --- |
| `AZURE_TENANT_ID` | the directory (tenant) ID |
| `AZURE_CLIENT_ID` | the app registration's client ID |
| `AZURE_CLIENT_SECRET` | the app registration's client secret |
| `AZURE_SIGNING_ENDPOINT` | the account's endpoint, e.g. `https://eus.codesigning.azure.net` |
| `AZURE_SIGNING_ACCOUNT` | the Trusted Signing account name |
| `AZURE_SIGNING_PROFILE` | the certificate profile name |

**Other routes.** Open-source projects can apply to the **SignPath Foundation** for free code
signing; it signs the finished installer in a separate step, so it needs its own workflow job
once a project is approved. A classic OV certificate (for example Certum's open-source
certificate) also works, but its key lives on a smart card or cloud HSM, which suits signing on
your own PC rather than in CI.

Signing doesn't make SmartScreen disappear on day one: Windows also weighs a publisher's
download reputation, which builds up over the first downloads of signed releases.

## After the first signed release

Once you've checked a signed build on each OS:

- Remove the "unsigned beta" line from the release body in `.github/workflows/release.yml` and
  from `docs/releases/<tag>.md`.
- Update the "First time opening it?" note on the website (`apps/web/index.html`).
- macOS: when updating from an unsigned build, remove the old Mozhi entry under Privacy &
  Security → Accessibility once and add the signed app. From then on, the permission carries over.
