/* Dashboard health policy, evaluated in one place.
   A rule comes from the target's config as the backend serves it:
   the normalized field key (lowercase, underscores stripped), one
   comparison, and the bound. A bare name in config is served as
   { op: "ne", value: 0 }. Policy lives in per-target config, never
   here. */

export interface HealthRule {
  field: string;
  op: string;
  value: number;
}

/** Normalize a field name the way rules are keyed. */
export function healthKey(name: string): string {
  return name.toLowerCase().replace(/_/g, "");
}

/** True when any rule for this field flags the value as bad. An
 *  unknown operator never flags: a rule the UI cannot read is not a
 *  reason to paint a card red. */
export function isBadValue(
  fieldName: string,
  value: number,
  rules: readonly HealthRule[],
): boolean {
  const key = healthKey(fieldName);
  for (const r of rules) {
    if (r.field !== key) continue;
    switch (r.op) {
      case "eq":
        if (value === r.value) return true;
        break;
      case "ne":
        if (value !== r.value) return true;
        break;
      case "ge":
        if (value >= r.value) return true;
        break;
      case "gt":
        if (value > r.value) return true;
        break;
      case "le":
        if (value <= r.value) return true;
        break;
      case "lt":
        if (value < r.value) return true;
        break;
      default:
        break;
    }
  }
  return false;
}
