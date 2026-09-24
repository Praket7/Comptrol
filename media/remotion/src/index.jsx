import React from 'react';
import {Composition, registerRoot, useCurrentFrame, interpolate, spring, useVideoConfig} from 'remotion';

const colors = {bg: '#080d15', panel: '#111a27', line: '#243348', white: '#f2f6fc', muted: '#9aabc0', blue: '#66b8ff', green: '#45d39a', amber: '#ffcb70'};

function Text({children, size = 24, color = colors.white, weight = 500, style = {}}) {
  return <div style={{fontFamily: 'Arial, sans-serif', fontSize: size, color, fontWeight: weight, lineHeight: 1.25, ...style}}>{children}</div>;
}

function Panel({x, y, w, h, children, delay = 0, frame}) {
  const opacity = interpolate(frame, [delay, delay + 18], [0, 1], {extrapolateRight: 'clamp'});
  const yShift = interpolate(frame, [delay, delay + 18], [18, 0], {extrapolateRight: 'clamp'});
  return <div style={{position: 'absolute', left: x, top: y + yShift, width: w, height: h, opacity, boxSizing: 'border-box', border: `1px solid ${colors.line}`, borderRadius: 22, background: colors.panel, padding: 28}}>{children}</div>;
}

function ComptrolDemo() {
  const frame = useCurrentFrame();
  const {fps} = useVideoConfig();
  const pulse = 0.82 + Math.sin(frame / 8) * 0.08;
  const progress = spring({frame: frame - 12, fps, config: {damping: 20, stiffness: 60}});
  const steps = [
    ['01', 'Check permission', 'The request fits local policy', colors.blue],
    ['02', 'Use the target', 'Comptrol acts through the browser fixture', colors.amber],
    ['03', 'Verify the result', 'Independent page state confirms success', colors.green],
  ];
  return <div style={{width: 1920, height: 1080, position: 'relative', overflow: 'hidden', background: colors.bg, color: colors.white}}>
    <div style={{position: 'absolute', inset: 0, background: 'radial-gradient(circle at 18% 18%, #173152 0, transparent 34%), radial-gradient(circle at 84% 72%, #0e332e 0, transparent 28%)'}} />
    <Text size={27} color={colors.blue} weight={700} style={{position: 'absolute', left: 110, top: 72, letterSpacing: 5}}>COMPTROL</Text>
    <Text size={64} weight={700} style={{position: 'absolute', left: 110, top: 142}}>A careful path from request to result</Text>
    <Text size={28} color={colors.muted} style={{position: 'absolute', left: 112, top: 232}}>Local computer control with permission checks and proof of outcome</Text>
    <Panel x={110} y={350} w={890} h={480} delay={4} frame={frame}>
      <Text size={22} color={colors.muted} weight={700}>A BROWSER TASK</Text>
      <Text size={34} weight={700} style={{marginTop: 18}}>Open a page and confirm it is ready</Text>
      <div style={{height: 2, background: colors.line, margin: '30px 0', transform: `scaleX(${progress})`, transformOrigin: 'left'}} />
      {steps.map(([number, title, detail, color], index) => {
        const active = interpolate(frame, [index * 34 + 14, index * 34 + 30], [0.35, 1], {extrapolateRight: 'clamp'});
        return <div key={number} style={{display: 'flex', alignItems: 'center', gap: 20, padding: '15px 0', opacity: active}}>
          <div style={{width: 50, height: 50, borderRadius: 25, display: 'grid', placeItems: 'center', background: `${color}20`, border: `1px solid ${color}`, color, fontSize: 19, fontWeight: 700}}>{number}</div>
          <div><Text size={25} weight={700}>{title}</Text><Text size={19} color={colors.muted} style={{marginTop: 4}}>{detail}</Text></div>
          <div style={{marginLeft: 'auto', color, fontSize: 24, opacity: active > 0.85 ? 1 : 0}}>✓</div>
        </div>;
      })}
      <div style={{position: 'absolute', right: 28, bottom: 24, padding: '8px 14px', borderRadius: 20, color: colors.green, background: '#14352e', fontSize: 18, opacity: interpolate(frame, [90, 112], [0, 1], {extrapolateRight: 'clamp'})}}>Verified</div>
    </Panel>
    <Panel x={1040} y={350} w={770} h={480} delay={24} frame={frame}>
      <Text size={22} color={colors.muted} weight={700}>LOCAL FIXTURE RESULTS</Text>
      <Text size={26} color={colors.muted} style={{marginTop: 8}}>3 browser tasks, 3 runs each</Text>
      <div style={{display: 'flex', gap: 18, marginTop: 32}}>
        {[['9 / 9', 'verified runs', colors.green], ['0', 'retries', colors.blue], ['0', 'false positives', colors.amber]].map(([value, label, color]) => <div key={label} style={{flex: 1, padding: '24px 18px', borderRadius: 16, background: '#0b121d', border: `1px solid ${colors.line}`, textAlign: 'center'}}><Text size={41} color={color} weight={700}>{value}</Text><Text size={17} color={colors.muted} style={{marginTop: 10}}>{label}</Text></div>)}
      </div>
      <Text size={20} color={colors.muted} style={{marginTop: 34}}>No foreground, mouse, or clipboard disturbance</Text>
      <Text size={18} color={colors.muted} style={{position: 'absolute', left: 28, bottom: 25}}>Local browser fixture on macOS ARM64</Text>
    </Panel>
    <div style={{position: 'absolute', left: 110, bottom: 110, width: 1700, height: 4, background: colors.line, borderRadius: 5}}>
      <div style={{height: 4, width: `${Math.min(frame / 179, 1) * 100}%`, background: colors.blue, borderRadius: 5, opacity: pulse}} />
    </div>
    <Text size={17} color={colors.muted} style={{position: 'absolute', left: 110, bottom: 62}}>Animated explanation. Fixture results are not a promise of performance in other apps.</Text>
  </div>;
}

function RemotionRoot() {
  return <Composition id="ComptrolDemo" component={ComptrolDemo} durationInFrames={180} fps={30} width={1920} height={1080} />;
}

registerRoot(RemotionRoot);
