#include "pch.h"
#include "service/SketchHelpers.h"
#include <algorithm>
#include <iostream>

namespace
{
	bool Has(const std::vector<Eigen::Vector2d>& points, const Eigen::Vector2d& p)
	{
		return std::any_of(points.begin(), points.end(), [&](const Eigen::Vector2d& q) { return (q - p).norm() < 1e-12; });
	}
}

// An arc's bounding points are its ends and the quadrant points it passes,
// going its own way round.
bool TestArcBoundingPoints()
{
	using sld_mcp::service::ArcBoundingPoints;
	const Eigen::Vector2d c(1.0, 2.0);
	const Eigen::Vector2d east = c + Eigen::Vector2d(2.0, 0.0), north = c + Eigen::Vector2d(0.0, 2.0);
	const Eigen::Vector2d west = c + Eigen::Vector2d(-2.0, 0.0), south = c + Eigen::Vector2d(0.0, -2.0);
	const double s = std::sqrt(2.0);
	const Eigen::Vector2d northEast = c + Eigen::Vector2d(s, s), northWest = c + Eigen::Vector2d(-s, s);

	// A quarter counterclockwise from east to north passes no other quadrant.
	const auto quarter = ArcBoundingPoints(c, 2.0, east, north, false);
	const bool quarterOk = Has(quarter, east) && Has(quarter, north) && !Has(quarter, west) && !Has(quarter, south);
	// The same ends clockwise go the long way round, through south and west.
	const auto longWay = ArcBoundingPoints(c, 2.0, east, north, true);
	const bool longWayOk = Has(longWay, west) && Has(longWay, south);
	// From north-east to north-west counterclockwise: only north between.
	const auto top = ArcBoundingPoints(c, 2.0, northEast, northWest, false);
	const bool topOk = top.size() == 3 && Has(top, north) && !Has(top, east) && !Has(top, west);

	if (!quarterOk || !longWayOk || !topOk)
	{
		std::cerr << "FAIL: an arc's bounding points (ends and the quadrant points it passes)\n";
		return false;
	}
	std::cout << "PASS: an arc's bounding points are its ends and the quadrant points it passes\n";
	return true;
}
