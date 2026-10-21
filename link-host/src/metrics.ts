// Counters in the Prometheus text format. Labels are always from a fixed set (a route's name, an
// outcome), never from a request, so the number of series cannot grow with the traffic.

export interface Metrics {
  count(name: string, labels?: Record<string, string | number>, by?: number): void;
  set(name: string, value: number): void;
  render(): string;
}

export function createMetrics(): Metrics {
  const counters = new Map<string, number>();
  const gauges = new Map<string, number>();
  const series = (name: string, labels: Record<string, string | number>) => {
    const pairs = Object.entries(labels).map(([label, value]) => `${label}="${String(value).replace(/["\\\n]/g, '')}"`);
    return pairs.length ? `${name}{${pairs.join(',')}}` : name;
  };
  return {
    count(name, labels = {}, by = 1) {
      const key = series(name, labels);
      counters.set(key, (counters.get(key) ?? 0) + by);
    },
    set(name, value) {
      gauges.set(name, value);
    },
    render() {
      const lines = [
        ...[...counters].sort(([a], [b]) => a.localeCompare(b)).map(([key, value]) => `${key} ${value}`),
        ...[...gauges].sort(([a], [b]) => a.localeCompare(b)).map(([key, value]) => `${key} ${value}`),
      ];
      return `${lines.join('\n')}\n`;
    },
  };
}
