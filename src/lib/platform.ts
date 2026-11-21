/*
 * Which kind of system the window runs on, where the screen follows that system's conventions
 * (window controls, shortcut labels). Read from the web view's user agent: WKWebView on macOS
 * reports "Macintosh", WebView2 on Windows does not.
 */

/** Pure: whether a user agent string is a Mac's. */
export function isMacUserAgent(userAgent: string): boolean {
  return /Macintosh|Mac OS X/.test(userAgent);
}

/** Whether this window runs on macOS. */
export const onMac: boolean = typeof navigator !== "undefined" && isMacUserAgent(navigator.userAgent);
