// Dates and bylines for articles (src/content/docs/docs/articles/). Dates are calendar days, so they
// are written in UTC: "2026-12-14" shows as 14 December 2026 wherever the site is built.
const format = new Intl.DateTimeFormat('en-GB', { day: 'numeric', month: 'long', year: 'numeric', timeZone: 'UTC' });

export const showDate = (date: Date) => format.format(date);
export const isoDate = (date: Date) => date.toISOString().slice(0, 10);
