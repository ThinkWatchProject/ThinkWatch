// Opening a link that belongs to an app (`thinkwatch://…`) without leaving
// the page.
//
// Following such a link in the page itself is a navigation of the page:
// where no app takes the scheme, Firefox and Safari replace the page with
// an error page, and a screen that shows an API key once loses the key.
// Loaded in a hidden iframe instead, the link reaches the app all the same
// when one is installed, and an error page stays inside the iframe, where
// no one sees it.

const FRAME_ID = 'tw-app-link-frame';

/// Opens `link` through a hidden iframe made for it. Call it from the
/// click that asks for it: browsers hand a link to an app only on a user's
/// action. The iframe stays until the next call replaces it — removing it
/// sooner could cancel the browser's "open this app?" prompt.
///
/// The console's Content-Security-Policy must allow the scheme in
/// `frame-src` (deploy/docker/nginx.conf.template).
export function openAppLink(link: string, doc: Document = document): void {
  doc.getElementById(FRAME_ID)?.remove();
  const frame = doc.createElement('iframe');
  frame.id = FRAME_ID;
  frame.hidden = true;
  frame.tabIndex = -1;
  frame.setAttribute('aria-hidden', 'true');
  // Set before the iframe joins the document, so that loading it replaces
  // its initial blank page instead of adding an entry to the tab's history.
  frame.src = link;
  doc.body.appendChild(frame);
}
