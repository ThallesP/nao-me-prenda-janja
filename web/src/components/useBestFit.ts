import { useEffect, useState } from "react";

/**
 * Discord-style grid fit: given N 16:9 tiles in a container, pick the column
 * count that maximizes tile area. Tiles get explicit sizes and the last row
 * stays centered (flex-wrap + justify-content: center).
 *
 * The element is tracked as state (callback ref) so the ResizeObserver
 * re-attaches when the grid unmounts (focus mode) and comes back.
 */
export const useBestFit = (count: number, gap = 8) => {
  const [el, setEl] = useState<HTMLDivElement | null>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });

  useEffect(() => {
    if (!el || count === 0) return;
    const aspect = 16 / 9;
    const compute = () => {
      const totalW = el.clientWidth;
      const totalH = el.clientHeight;
      let bestW = 0;
      for (let cols = 1; cols <= count; cols++) {
        const rows = Math.ceil(count / cols);
        const fitW = (totalW - gap * (cols - 1)) / cols;
        const fitH = ((totalH - gap * (rows - 1)) / rows) * aspect;
        const width = Math.min(fitW, fitH);
        if (width > bestW) bestW = width;
      }
      setSize({ width: Math.floor(bestW), height: Math.floor(bestW / aspect) });
    };
    const observer = new ResizeObserver(compute);
    observer.observe(el);
    compute();
    return () => observer.disconnect();
  }, [el, count, gap]);

  return { ref: setEl, size };
};
