/**
 * Link previews. The dashboard is private, but a pasted link should still unfurl on LinkedIn, Slack and the rest: the
 * login page carries the Open Graph card (no private data), and these crawlers are let through to it.
 */

// Unfurlers that read Open Graph tags, matched case-insensitively on a substring of the User-Agent.
const PREVIEW_BOTS =
  /linkedinbot|twitterbot|facebookexternalhit|facebot|slackbot|discordbot|whatsapp|telegrambot|skypeuripreview|applebot|redditbot|embedly|iframely|pinterest/i;

export function isPreviewBot(request: Request): boolean {
  return PREVIEW_BOTS.test(request.headers.get("User-Agent") ?? "");
}
