/* An integer field's value as the operator types it: one rule for every
 * form that takes an integer as text, read exactly at any width. */

/** The values an integer field `bits` wide holds. */
function integerRange(
  bits: number,
  signed: boolean,
): { min: bigint; max: bigint } {
  const width = BigInt(bits);
  return signed
    ? { min: -(1n << (width - 1n)), max: (1n << (width - 1n)) - 1n }
    : { min: 0n, max: (1n << width) - 1n };
}

/** `text` as the whole number an integer field `bits` wide takes, or
 *  null. The field takes decimal digits, with a leading minus when it is
 *  signed, or 0x and hex digits; whitespace around them is ignored. The
 *  value is read as a BigInt, exact at any width (a double holds
 *  integers exactly only up to 2^53), and must lie in the field's range.
 *  A blank, a sign on hex, a fraction, an exponent and any other text
 *  are null. */
export function parseWholeNumber(
  text: string,
  bits: number,
  signed: boolean,
): bigint | null {
  const t = text.trim();
  const decimal = signed ? /^-?[0-9]+$/ : /^[0-9]+$/;
  if (!decimal.test(t) && !/^0[xX][0-9a-fA-F]+$/.test(t)) return null;
  const n = BigInt(t);
  const { min, max } = integerRange(bits, signed);
  return n >= min && n <= max ? n : null;
}

/** What an integer field `bits` wide takes, in the words a refusal
 *  uses: "a whole number from 0 to 255, decimal or 0x hex". */
export function wholeNumberHint(bits: number, signed: boolean): string {
  const { min, max } = integerRange(bits, signed);
  return `a whole number from ${min} to ${max}, decimal or 0x hex`;
}
