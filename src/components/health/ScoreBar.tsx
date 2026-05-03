/// Small score meter displayed in the health panel header.
export function ScoreBar({ score }: { score: number }) {
  const color =
    score >= 90 ? 'bg-green-500' :
    score >= 70 ? 'bg-yellow-500' :
    score >= 50 ? 'bg-orange-500' : 'bg-red-500';
  return (
    <div className="w-full bg-[#3a3a3a] rounded-full h-1.5">
      <div
        className={`${color} h-1.5 rounded-full transition-all duration-500`}
        style={{ width: `${score}%` }}
      />
    </div>
  );
}
