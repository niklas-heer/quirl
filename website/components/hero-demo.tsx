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
        void video.play();
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
      loop
      muted
      playsInline
      aria-label="Recorded Quirl terminal session: a native pipeline and semantic completion, typed JSON transformed with open, where, sort, and select, local AI suggestions, explicit Bash compatibility, sandboxed Lua, and measured release proof"
    >
      <source src="/quirl-demo.webm" type="video/webm" />
      <source src="/quirl-demo.mp4" type="video/mp4" />
    </video>
  );
}
