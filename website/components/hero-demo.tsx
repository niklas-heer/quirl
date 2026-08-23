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
      aria-label="Recorded Quirl terminal session: semantic completion, a native pipeline, history replay, a typed data filter, and an explicit Bash dialect island"
    >
      <source src="/quirl-demo.webm" type="video/webm" />
      <source src="/quirl-demo.mp4" type="video/mp4" />
    </video>
  );
}
