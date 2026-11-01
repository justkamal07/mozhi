// The one place that knows where Mozhi lives on the internet.
export const REPO_URL = "https://github.com/Kamal007OLica/mozhi";
// The release the download buttons describe. CI attaches stable-named installers to every
// full release, so /releases/latest/download/<name> always serves the newest one.
export const VERSION = "0.3.0";

const LATEST = `${REPO_URL}/releases/latest/download`;

export const SITE = {
  // TODO: switch to the custom domain once there is one.
  url: "https://mozhi-khaki.vercel.app",
  repo: REPO_URL,
  // Every version and its notes.
  releases: `${REPO_URL}/releases`,
  issues: `${REPO_URL}/issues/new/choose`,
  contributing: `${REPO_URL}/blob/main/CONTRIBUTING.md`,
  license: `${REPO_URL}/blob/main/LICENSE`,
  buildFromSource: `${REPO_URL}#building-from-source`,
  version: VERSION,
  dlMac: `${LATEST}/Mozhi-macOS-AppleSilicon.dmg`,
  dlWin: `${LATEST}/Mozhi-Windows-x64-setup.exe`,
  dlWinMsi: `${LATEST}/Mozhi-Windows-x64.msi`,
};
