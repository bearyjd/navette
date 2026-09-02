package com.greponlabs.navette.ui.theme

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

private val NavetteTeal = Color(0xFF7CE0C6)
private val NavetteInk = Color(0xFF1B1F3B)

private val DarkColors =
    darkColorScheme(
        primary = NavetteTeal,
        background = NavetteInk,
        surface = NavetteInk,
    )

private val LightColors =
    lightColorScheme(
        primary = NavetteInk,
        secondary = NavetteTeal,
    )

@Composable
fun NavetteTheme(
    darkTheme: Boolean = isSystemInDarkTheme(),
    content: @Composable () -> Unit,
) {
    val colorScheme = if (darkTheme) DarkColors else LightColors
    MaterialTheme(colorScheme = colorScheme, content = content)
}
