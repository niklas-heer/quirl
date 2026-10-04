'use client';

import { useEffect, useRef } from 'react';

export function HeroDemo() {
  const videoRef = useRef<HTMLVideoElement>(null);

  useEffect(() => {
    const video = videoRef.current;
    if (!video) return;
    const reduceMotion = window.matchMedia('(prefers-reduced-motion: reduce)');
    const apply = () => {
      if (reduceMotion.matches) {
        video.pause();
        video.removeAttribute('loop');
      } else {
        video.setAttribute('loop', '');
        // Browser autoplay policies vary; native controls remain available
        // when playback cannot start without an explicit user gesture.
        void video.play().catch(() => undefined);
      }
    };
    apply();
    reduceMotion.addEventListener('change', apply);
    return () => reduceMotion.removeEventListener('change', apply);
  }, []);

  return (
    <video
      ref={videoRef}
      className="terminal-video"
      poster="/quirl-demo-poster.png"
      autoPlay
      controls
      loop
      muted
      preload="auto"
      playsInline
      width={1200}
      height={720}
      aria-label="Quirl v0.5.1 demo: familiar commands with project-aware completion, pasted Bash loops and setup scripts, typed data tables with sort-by, group-by, and math, and sandboxed Lua"
      aria-describedby="demo-recording-context"
    >
      <source src="/quirl-demo.webm" type="video/webm" />
      <source src="/quirl-demo.mp4" type="video/mp4" />
      <a href="/quirl-demo.mp4">Watch the Quirl product tour.</a>
    </video>
  );
}
