/** Card arrangement preference: pure application logic, kept out of
 *  the component so ordering/hiding semantics are unit-testable. */

export interface CardArrangement {
  /** Card titles hidden by the operator. */
  hidden: string[];
  /** Preferred order; cards absent from it (new components after a
   *  target refresh) append after, in their natural order -- which is
   *  what makes defaults regenerate cleanly for a fresh target. */
  order: string[];
}

export function arrangeCards<T extends { title: string }>(
  cards: T[],
  pref: CardArrangement | null,
  showHidden: boolean,
): T[] {
  if (!pref) return cards;
  const rank = new Map(pref.order.map((t, i) => [t, i]));
  const hidden = new Set(pref.hidden);
  const sorted = [...cards].sort((a, b) => {
    const ra = rank.get(a.title);
    const rb = rank.get(b.title);
    if (ra !== undefined && rb !== undefined) return ra - rb;
    if (ra !== undefined) return -1;
    if (rb !== undefined) return 1;
    return 0; // both unknown: stable natural order
  });
  return showHidden ? sorted : sorted.filter((c) => !hidden.has(c.title));
}

export function moveTitle(
  order: string[],
  allTitles: string[],
  title: string,
  delta: -1 | 1,
): string[] {
  // Materialize the effective order (pref order then unranked) so a
  // first-ever move behaves predictably.
  const ranked = order.filter((t) => allTitles.includes(t));
  const rest = allTitles.filter((t) => !ranked.includes(t));
  const full = [...ranked, ...rest];
  const i = full.indexOf(title);
  const j = i + delta;
  if (i < 0 || j < 0 || j >= full.length) return full;
  [full[i], full[j]] = [full[j], full[i]];
  return full;
}

export function toggleHidden(hidden: string[], title: string): string[] {
  return hidden.includes(title)
    ? hidden.filter((t) => t !== title)
    : [...hidden, title];
}
