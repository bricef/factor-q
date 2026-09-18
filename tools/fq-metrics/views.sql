-- Each measure has an all-time and trailing-30-day row. Empty denominators yield NULL.
DROP VIEW IF EXISTS first_pass_rate;
CREATE VIEW first_pass_rate AS
WITH windows(name, cutoff) AS (VALUES ('30_days', datetime('now','-30 days')), ('all_time', NULL))
SELECT name AS window,
  1.0 * SUM(CASE WHEN o.accepted=1 AND NOT EXISTS (
    SELECT 1 FROM interventions i WHERE i.issue=a.issue
      AND i.at >= a.dispatched_at AND i.at <= COALESCE(a.ended_at, datetime('now'))
  ) THEN 1 ELSE 0 END) / NULLIF(COUNT(a.invocation_id),0) AS value
FROM windows LEFT JOIN attempts a ON cutoff IS NULL OR a.dispatched_at >= cutoff
LEFT JOIN outcomes o ON o.pr_number=a.pr_number GROUP BY name;

DROP VIEW IF EXISTS attempts_per_accept;
CREATE VIEW attempts_per_accept AS
WITH windows(name, cutoff) AS (VALUES ('30_days', datetime('now','-30 days')), ('all_time', NULL))
SELECT name AS window, 1.0 * COUNT(DISTINCT a.invocation_id) /
  NULLIF(COUNT(DISTINCT CASE WHEN o.accepted=1 THEN a.issue END),0) AS value
FROM windows LEFT JOIN attempts a ON cutoff IS NULL OR a.dispatched_at >= cutoff
LEFT JOIN outcomes o ON o.pr_number=a.pr_number GROUP BY name;

DROP VIEW IF EXISTS touch_per_accept;
CREATE VIEW touch_per_accept AS
WITH windows(name, cutoff) AS (VALUES ('30_days', datetime('now','-30 days')), ('all_time', NULL)),
totals AS (
 SELECT w.name, o.issue, SUM(COALESCE(i.minutes,0)) minutes
 FROM windows w JOIN outcomes o ON o.accepted=1 AND (w.cutoff IS NULL OR o.accepted_at >= w.cutoff)
 LEFT JOIN interventions i ON i.issue=o.issue GROUP BY w.name,o.issue
), ranked AS (
 SELECT name,minutes,ROW_NUMBER() OVER(PARTITION BY name ORDER BY minutes) n,
 COUNT(*) OVER(PARTITION BY name) count FROM totals
), medians AS (
 SELECT name,AVG(minutes) value FROM ranked
 WHERE n IN ((count+1)/2,(count+2)/2) GROUP BY name
)
SELECT windows.name AS window,medians.value FROM windows LEFT JOIN medians USING(name);

DROP VIEW IF EXISTS mtbi;
CREATE VIEW mtbi AS
WITH windows(name, cutoff) AS (VALUES ('30_days', datetime('now','-30 days')), ('all_time', NULL)),
gaps AS (
 SELECT w.name, (julianday(i.at)-julianday(LAG(i.at) OVER(PARTITION BY w.name ORDER BY i.at)))*24.0 hours
 FROM windows w JOIN interventions i ON w.cutoff IS NULL OR i.at >= w.cutoff
), means AS (SELECT name,AVG(hours) value FROM gaps WHERE hours IS NOT NULL GROUP BY name)
SELECT windows.name AS window,means.value FROM windows LEFT JOIN means USING(name);

DROP VIEW IF EXISTS correction_ratio;
CREATE VIEW correction_ratio AS
WITH windows(name, cutoff) AS (VALUES ('30_days', datetime('now','-30 days')), ('all_time', NULL)),
corrections AS (
 SELECT w.name, COALESCE(SUM(c.additions+c.deletions),0) loc FROM windows w
 LEFT JOIN corrective_commits c ON w.cutoff IS NULL OR c.at >= w.cutoff GROUP BY w.name
), agents AS (
 SELECT w.name, COALESCE(SUM(p.additions+p.deletions),0) loc FROM windows w
 LEFT JOIN pull_requests p ON p.agent_authored=1 AND (w.cutoff IS NULL OR p.merged_at >= w.cutoff)
 GROUP BY w.name
)
SELECT corrections.name AS window, 1.0*corrections.loc/NULLIF(agents.loc,0) AS value
FROM corrections JOIN agents USING(name);
